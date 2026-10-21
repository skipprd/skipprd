//! Buffered (page-cache) WAL scan for SQL. Durability code must not construct this.

use std::io::{self, Read, Seek};
use std::path::PathBuf;

use arrow::array::RecordBatch;
use arrow::ipc::reader::StreamReader;
use skippr_wal_catalog::{CatalogKey, CatalogRow, PayloadLocation};

use crate::buffer::segment_file::SegmentFile;
use crate::buffer::wal_handle;
use crate::buffer::wal_store::block_on_async;
use crate::helpers::configuration::Config;

pub trait WalReader {
    fn load_committed_batches(
        &self,
        pipeline: &str,
        limit_files: usize,
    ) -> io::Result<Vec<RecordBatch>>;
}

pub struct CatalogWalReader {
    key: skippr_lease::PipelineKey,
}

struct InvalidPipelineReader {
    message: String,
}

impl WalReader for InvalidPipelineReader {
    fn load_committed_batches(
        &self,
        _name: &str,
        _limit_files: usize,
    ) -> io::Result<Vec<RecordBatch>> {
        Err(io::Error::other(self.message.clone()))
    }
}

impl WalReader for CatalogWalReader {
    fn load_committed_batches(
        &self,
        name: &str,
        limit_files: usize,
    ) -> io::Result<Vec<RecordBatch>> {
        let handle = wal_handle::wal_handle_for(&self.key).ok_or_else(|| {
            io::Error::other(format!(
                "WAL read has no PipelineWalHandle for pipeline {}",
                self.key.pipeline()
            ))
        })?;
        let ordinals = handle.select_live_ordinals(&[]);
        let mut out = Vec::new();
        let mut used_segments = 0usize;
        let mut seen = std::collections::HashSet::new();
        for (segment_id, _ordinal) in ordinals {
            if used_segments >= limit_files {
                break;
            }
            if !seen.insert(segment_id.clone()) {
                continue;
            }
            let Ok(id) = skippr_lease::SegmentId::new(&segment_id) else {
                continue;
            };
            let Ok(Some(CatalogRow::Segment(seg))) = handle.catalog().get(&CatalogKey::Segment(id))
            else {
                continue;
            };
            let batches = match &seg.location {
                PayloadLocation::Disk { path } => {
                    load_disk_segment_batches(PathBuf::from(path), name, &seg.payload_sha256)?
                }
                PayloadLocation::S3 { bucket, key } => {
                    load_s3_segment_batches(bucket, key, name, &seg.payload_sha256)?
                }
            };
            if !batches.is_empty() {
                used_segments += 1;
            }
            out.extend(batches);
        }
        Ok(out)
    }
}

fn decode_arrow_parts(
    bytes: &[u8],
    namespace_or_pipeline: &str,
    expected_sha: &[u8; 32],
) -> io::Result<Vec<RecordBatch>> {
    let claimed = SegmentFile::verify_body_checksum(bytes)?;
    if &claimed != expected_sha {
        return Ok(Vec::new());
    }
    let meta = SegmentFile::read_metadata_from_bytes(bytes)?;
    let mut out = Vec::new();
    for idx in meta.index.iter() {
        if idx.key.namespace != namespace_or_pipeline {
            continue;
        }
        if idx.codec != crate::buffer::segment_file::PART_CODEC_ARROW {
            return Err(io::Error::other("sqlrt does not read copy WAL parts"));
        }
        let start = idx.start as usize;
        let end = start.saturating_add(idx.len as usize);
        if end > bytes.len() {
            continue;
        }
        let slice = &bytes[start..end];
        let mut cursor = std::io::Cursor::new(slice);
        if let Ok(sr) = StreamReader::try_new(&mut cursor, None) {
            for it in sr {
                if let Ok(b) = it {
                    out.push(b);
                }
            }
        }
    }
    Ok(out)
}

fn load_disk_segment_batches(
    path: PathBuf,
    namespace_or_pipeline: &str,
    expected_sha: &[u8; 32],
) -> io::Result<Vec<RecordBatch>> {
    let Ok(meta) = SegmentFile::admit_payload_path(&path, expected_sha) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for idx in meta.index.iter() {
        if idx.key.namespace != namespace_or_pipeline {
            continue;
        }
        if idx.codec != crate::buffer::segment_file::PART_CODEC_ARROW {
            return Err(io::Error::other("sqlrt does not read copy WAL parts"));
        }
        let mut file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        if file.seek(std::io::SeekFrom::Start(idx.start)).is_err() {
            continue;
        }
        let reader = std::io::BufReader::new(file);
        let mut take = reader.take(idx.len);
        if let Ok(sr) = StreamReader::try_new(&mut take, None) {
            for it in sr {
                if let Ok(b) = it {
                    out.push(b);
                }
            }
        }
    }
    Ok(out)
}

fn load_s3_segment_batches(
    bucket: &str,
    key: &str,
    namespace_or_pipeline: &str,
    expected_sha: &[u8; 32],
) -> io::Result<Vec<RecordBatch>> {
    let bucket = bucket.to_string();
    let key = key.to_string();
    let filter = namespace_or_pipeline.to_string();
    let sha = *expected_sha;
    block_on_async(async move {
        let bytes = crate::helpers::s3::get_object_bytes_for_bucket(&bucket, &key)
            .await
            .map_err(|err| io::Error::other(err.to_string()))?;
        decode_arrow_parts(&bytes, &filter, &sha)
    })
}

pub struct WalReaderFactory;

impl WalReaderFactory {
    pub fn for_pipeline(config: &Config, pipeline: &str) -> Box<dyn WalReader + Send + Sync> {
        match skippr_lease::PipelineKey::new(
            config.get_tenant(),
            config.get_workspace_name(),
            pipeline,
        ) {
            Ok(key) => Box::new(CatalogWalReader { key }),
            Err(err) => Box::new(InvalidPipelineReader {
                message: err.to_string(),
            }),
        }
    }

    pub async fn for_pipeline_async(
        config: &Config,
        pipeline: &str,
    ) -> Box<dyn WalReader + Send + Sync> {
        Self::for_pipeline(config, pipeline)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::segment_file::{PartitionKey, WalPayload};
    use crate::buffer::wal_handle::{
        clear_wal_handles, install_wal_handle, memory_catalog_for_test, remove_wal_handle,
        PipelineWalHandle,
    };
    use crate::helpers::offset_store::{OffsetStore, SledOffsetStore};
    use skippr_lease::{LeaseGuard, PipelineKey, PipelinePaths, SystemClock};
    use skippr_wal_catalog::CatalogFence;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn leased_handle(dir: &std::path::Path, pipeline: &str) -> Arc<PipelineWalHandle> {
        let key = PipelineKey::new("t", "w", pipeline).unwrap();
        let paths = PipelinePaths::legacy_disk(dir);
        let clock = Arc::new(SystemClock::new());
        let guard = LeaseGuard::single_node(key.clone(), clock);
        let fence = crate::buffer::wal_handle::catalog_fence_from_guard(&guard).unwrap();
        let catalog = memory_catalog_for_test(fence);
        let db = sled::open(dir.join("offsets")).unwrap();
        let tree = db.open_tree("offsets").unwrap();
        let offsets: Arc<dyn OffsetStore> = Arc::new(SledOffsetStore::new(tree));
        PipelineWalHandle::new(key, paths, guard, catalog, offsets)
    }

    #[test]
    #[serial_test::serial]
    fn clustered_wal_reader_does_not_fall_back_to_another_pipeline() {
        let dir = tempfile::tempdir().unwrap();
        let handle = leased_handle(dir.path(), "other");
        install_wal_handle(handle.clone());
        let err = CatalogWalReader {
            key: PipelineKey::new("t", "w", "missing").unwrap(),
        }
        .load_committed_batches("missing", 1)
        .unwrap_err();
        remove_wal_handle(handle.key());
        clear_wal_handles();
        assert!(err.to_string().contains("missing"));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn catalog_wal_reader_loads_persisted_segment() {
        let dir = tempfile::tempdir().unwrap();
        let handle = leased_handle(dir.path(), "orders");
        install_wal_handle(handle.clone());
        let schema = Arc::new(arrow::datatypes::Schema::new(vec![
            arrow::datatypes::Field::new("id", arrow::datatypes::DataType::Utf8, true),
        ]));
        let mut batches = HashMap::new();
        batches.insert(
            PartitionKey {
                sink_ref: String::new(),
                namespace: "orders_el".into(),
                partition: "p0".into(),
                time: None,
                schema_fingerprint: "fp".into(),
            },
            WalPayload::Arrow(vec![arrow::array::RecordBatch::try_new(
                schema,
                vec![Arc::new(arrow::array::StringArray::from(vec!["row-1"]))],
            )
            .unwrap()]),
        );
        handle
            .persist_segment(
                "file-seg",
                &HashMap::new(),
                &batches,
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
            )
            .await
            .unwrap();
        let loaded = CatalogWalReader {
            key: PipelineKey::new("t", "w", "orders").unwrap(),
        }
        .load_committed_batches("orders_el", usize::MAX)
        .unwrap();
        remove_wal_handle(handle.key());
        clear_wal_handles();
        assert_eq!(loaded.iter().map(|b| b.num_rows()).sum::<usize>(), 1);
    }
}
