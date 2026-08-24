//! Buffered (page-cache) WAL scan for SQL. Durability code must not construct this.

use std::fs;
use std::io::{self, Read, Seek};
use std::path::PathBuf;

use arrow::array::RecordBatch;
use arrow::ipc::reader::StreamReader;
use url::Url;

use crate::buffer::segment_file::SegmentFile;
use crate::buffer::wal_store::block_on_async;
use crate::helpers::configuration::Config;
use crate::helpers::wal_storage::WalStorage;

pub trait WalReader {
    fn load_committed_batches(
        &self,
        pipeline: &str,
        limit_files: usize,
    ) -> io::Result<Vec<RecordBatch>>;
}

pub struct ClusteredWalReader;

impl WalReader for ClusteredWalReader {
    fn load_committed_batches(
        &self,
        name: &str,
        limit_files: usize,
    ) -> io::Result<Vec<RecordBatch>> {
        let stores = crate::buffer::durable::all_durable_stores();
        let store = stores
            .iter()
            .find(|store| store.key().pipeline() == name)
            .cloned()
            .ok_or_else(|| {
                io::Error::other(format!(
                    "clustered WAL read has no PipelineDurableStore for pipeline {name}"
                ))
            })?;
        load_seg_dir_batches(store.paths().segs.clone(), name, limit_files)
    }
}

fn load_seg_dir_batches(
    seg_dir: PathBuf,
    namespace_or_pipeline: &str,
    limit_files: usize,
) -> io::Result<Vec<RecordBatch>> {
    let mut out: Vec<RecordBatch> = Vec::new();
    if !seg_dir.exists() {
        return Ok(out);
    }
    let mut used = 0usize;
    for entry in fs::read_dir(&seg_dir)? {
        if used >= limit_files {
            break;
        }
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("seg") {
            continue;
        }
        let commit = path.with_extension("seg.commit");
        if !commit.exists() {
            continue;
        }
        let mut meta_file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(_) => continue,
        };
        let meta = match SegmentFile::read_metadata_from_reader(&mut meta_file) {
            Ok(m) => m,
            Err(_) => continue,
        };
        for idx in meta.index.iter() {
            if idx.key.namespace != namespace_or_pipeline {
                continue;
            }
            let mut file = match fs::File::open(&path) {
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
        used += 1;
        if used >= limit_files {
            break;
        }
    }
    Ok(out)
}

pub struct S3WalReader {
    pub prefix_url: String,
}

impl WalReader for S3WalReader {
    fn load_committed_batches(
        &self,
        pipeline: &str,
        limit_files: usize,
    ) -> io::Result<Vec<RecordBatch>> {
        let prefix = self.prefix_url.clone();
        let pipe = pipeline.to_string();
        let fut = async move {
            let mut out: Vec<RecordBatch> = Vec::new();
            let u = match Url::parse(&prefix) {
                Ok(u) => u,
                Err(_) => return Ok(out),
            };
            if u.scheme() != "s3" {
                return Ok(out);
            }
            let bucket = match u.host_str() {
                Some(b) => b.to_string(),
                None => return Ok(out),
            };
            let base = u
                .path()
                .trim_start_matches('/')
                .trim_end_matches('/')
                .to_string();
            let client = crate::helpers::s3::get_s3_client().await;
            let mut token: Option<String> = None;
            let mut all: Vec<String> = Vec::new();
            loop {
                let mut req = client.list_objects_v2().bucket(&bucket).prefix(&base);
                if let Some(t) = &token {
                    req = req.continuation_token(t);
                }
                match req.send().await {
                    Ok(resp) => {
                        if let Some(contents) = resp.contents {
                            for obj in contents {
                                if let Some(k) = obj.key() {
                                    all.push(k.to_string());
                                }
                            }
                        }
                        if resp.is_truncated.unwrap_or(false) {
                            token = resp.next_continuation_token;
                        } else {
                            break;
                        }
                    }
                    Err(_) => break,
                }
                if all.len() > 10_000 {
                    break;
                }
            }
            use std::collections::HashSet;
            let set: HashSet<String> = all.iter().cloned().collect();
            let mut used = 0usize;
            for k in all.into_iter().filter(|k| k.ends_with(".seg")) {
                if used >= limit_files {
                    break;
                }
                let commit = format!("{}.commit", k);
                if !set.contains(&commit) {
                    continue;
                }
                match client.get_object().bucket(&bucket).key(&k).send().await {
                    Ok(resp) => match resp.body.collect().await {
                        Ok(agg) => {
                            let bytes = agg.into_bytes().to_vec();
                            if let Ok(meta) = SegmentFile::read_metadata_from_bytes(&bytes) {
                                for idx in meta.index.iter() {
                                    if idx.key.namespace != pipe {
                                        continue;
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
                                used += 1;
                            }
                        }
                        Err(_) => {}
                    },
                    Err(_) => {}
                }
            }
            Ok(out)
        };
        block_on_async(fut)
    }
}

pub struct WalReaderFactory;

impl WalReaderFactory {
    pub fn for_pipeline(_pipeline: &str) -> Box<dyn WalReader + Send + Sync> {
        match Config::get_wal_storage() {
            WalStorage::S3 => {
                let bucket = Config::get_wal_s3_bucket();
                let base = Config::get_wal_s3_prefix();
                let prefix_url = format!("s3://{}/{}", bucket, base.trim_start_matches('/'));
                Box::new(S3WalReader { prefix_url })
            }
            WalStorage::Disk | WalStorage::Clustered => Box::new(ClusteredWalReader),
        }
    }

    pub async fn for_pipeline_async(_pipeline: &str) -> Box<dyn WalReader + Send + Sync> {
        Self::for_pipeline(_pipeline)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::durable::log::MutationLog;
    use crate::buffer::durable::replicate::ReplicationMode;
    use crate::buffer::durable::store::{
        install_durable_store, remove_durable_store, MemoryOffsetPublisher, OffsetMode,
        PipelineDurableStore,
    };
    use skippr_lease::{LeaseGuard, PipelineKey, PipelinePaths, SystemClock};
    use std::sync::Arc;

    #[test]
    #[serial_test::serial]
    fn clustered_wal_reader_does_not_fall_back_to_another_pipeline() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "other").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        let guard = LeaseGuard::single_node(key.clone(), Arc::new(SystemClock::new()));
        let log = MutationLog::open(paths.clone()).unwrap();
        let store = PipelineDurableStore::new(
            key.clone(),
            paths,
            guard,
            log,
            ReplicationMode::LocalOnly,
            OffsetMode::Dynamo(MemoryOffsetPublisher::new()),
        );
        install_durable_store(store);
        let err = ClusteredWalReader
            .load_committed_batches("missing", 1)
            .unwrap_err();
        remove_durable_store(&key);
        assert!(err.to_string().contains("missing"));
    }
}
