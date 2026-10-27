use crate::buffer::segment_file::{SegmentFile, WalPayload, PART_CODEC_ARROW, PART_CODEC_COPY};
use crate::discover::schema_alter::SchemaAlterOp;
use arrow::array::RecordBatch;
use arrow::datatypes::{Field, Schema};
use arrow::ipc::reader::StreamReader;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

const FIELD_ID_KEYS: [&str; 2] = ["PARQUET:field_id", "iceberg.field-id"];

pub fn rewrite_record_batch(
    batch: RecordBatch,
    op: &SchemaAlterOp,
    field_id: i32,
) -> Result<RecordBatch, String> {
    let schema = rewrite_schema(batch.schema_ref().as_ref(), op, field_id)?;
    RecordBatch::try_new(Arc::new(schema), batch.columns().to_vec()).map_err(|err| err.to_string())
}

pub fn rewrite_schema(
    schema: &Schema,
    op: &SchemaAlterOp,
    field_id: i32,
) -> Result<Schema, String> {
    let id_token = field_id.to_string();
    let fields: Vec<Field> = schema
        .fields()
        .iter()
        .map(|field| rewrite_field(field.as_ref(), op, &id_token))
        .collect();
    Ok(Schema::new_with_metadata(fields, schema.metadata().clone()))
}

fn rewrite_field(field: &Field, op: &SchemaAlterOp, id_token: &str) -> Field {
    let matches = field_has_id(field, id_token) || field.name() == lookup_name(op);
    if !matches {
        return field.clone();
    }
    match op {
        SchemaAlterOp::Rename { to, .. } => field.clone().with_name(iceberg_flatten(to)),
        SchemaAlterOp::Promote { .. } => field.clone(),
        SchemaAlterOp::Drop { .. } | SchemaAlterOp::Merge { .. } => field.clone(),
    }
}

fn lookup_name(op: &SchemaAlterOp) -> &str {
    match op {
        SchemaAlterOp::Rename { from, .. } => from.as_str(),
        SchemaAlterOp::Merge { src, .. } => src.as_str(),
        SchemaAlterOp::Drop { column } | SchemaAlterOp::Promote { column, .. } => column.as_str(),
    }
}

fn iceberg_flatten(name: &str) -> String {
    name.replace('.', "_").replace('-', "_")
}

fn field_has_id(field: &Field, id_token: &str) -> bool {
    FIELD_ID_KEYS
        .iter()
        .any(|key| field.metadata().get(*key).map(String::as_str) == Some(id_token))
}

/// Rewrite live Arrow WAL parts onto schema N+1. Copy/non-Arrow parts fail closed.
pub fn rewrite_live_wal(
    config: &crate::helpers::configuration::Config,
    namespace: &str,
    op: &SchemaAlterOp,
    field_id: i32,
) -> Result<(), String> {
    crate::buffer::ingest_buffer::Buffers::map_live_arrow(namespace, |batch| {
        rewrite_record_batch(batch, op, field_id)
    })?;
    let Some(handle) = crate::buffer::wal_store::ingest_wal_handle() else {
        return Ok(());
    };
    for seg in handle.live_segment_rows().map_err(|err| err.to_string())? {
        rewrite_segment_row(config, &seg, namespace, op, field_id)?;
    }
    Ok(())
}

fn rewrite_segment_row(
    config: &crate::helpers::configuration::Config,
    seg: &skippr_wal_catalog::SegmentRow,
    namespace: &str,
    op: &SchemaAlterOp,
    field_id: i32,
) -> Result<(), String> {
    match &seg.location {
        skippr_wal_catalog::PayloadLocation::Disk { path } => rewrite_segment_file(
            config,
            std::path::Path::new(path),
            seg.segment_id.as_str(),
            namespace,
            op,
            field_id,
        ),
        skippr_wal_catalog::PayloadLocation::S3 { .. } => {
            let bytes =
                crate::buffer::wal_handle::payload_bytes(&seg.location, &seg.payload_sha256)
                    .map_err(|err| err.to_string())?;
            let Some(encoded) = rewrite_wal_bytes(&bytes, namespace, op, field_id)? else {
                return Ok(());
            };
            persist_rewritten_payload(config, &seg.location, seg.segment_id.as_str(), encoded)
        }
    }
}

fn rewrite_segment_file(
    config: &crate::helpers::configuration::Config,
    path: &Path,
    segment_id: &str,
    namespace: &str,
    op: &SchemaAlterOp,
    field_id: i32,
) -> Result<(), String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(()),
    };
    let Some(encoded) = rewrite_wal_bytes(&bytes, namespace, op, field_id)? else {
        return Ok(());
    };
    persist_rewritten_payload(
        config,
        &skippr_wal_catalog::PayloadLocation::Disk {
            path: path.to_string_lossy().into_owned(),
        },
        segment_id,
        encoded,
    )
}

fn rewrite_wal_bytes(
    bytes: &[u8],
    namespace: &str,
    op: &SchemaAlterOp,
    field_id: i32,
) -> Result<Option<crate::buffer::segment_file::EncodedSnapshot>, String> {
    let meta = SegmentFile::read_metadata_from_bytes(bytes).map_err(|err| err.to_string())?;
    let mut batches: HashMap<_, WalPayload> = HashMap::new();
    let mut partitions_meta = HashMap::new();
    let mut part_meta_blobs = HashMap::new();
    let mut mutated = false;
    for idx in &meta.index {
        if idx.codec == PART_CODEC_COPY && idx.key.namespace == namespace {
            return Err("copy/non-Arrow WAL parts cannot be rewritten".to_string());
        }
        let start = idx.start as usize;
        let end = start.saturating_add(idx.len as usize);
        if end > bytes.len() {
            return Err("WAL part exceeds payload length".to_string());
        }
        let slice = &bytes[start..end];
        let payload = if idx.codec == PART_CODEC_ARROW {
            let mut cursor = std::io::Cursor::new(slice);
            let reader = StreamReader::try_new(&mut cursor, None).map_err(|err| err.to_string())?;
            let mut arrow_batches = Vec::new();
            for batch in reader {
                let batch = batch.map_err(|err| err.to_string())?;
                if idx.key.namespace == namespace {
                    arrow_batches.push(rewrite_record_batch(batch, op, field_id)?);
                    mutated = true;
                } else {
                    arrow_batches.push(batch);
                }
            }
            WalPayload::Arrow(arrow_batches)
        } else {
            WalPayload::Raw(
                crate::buffer::segment_file::decode_copy_part(slice)
                    .map_err(|err| err.to_string())?,
            )
        };
        if idx.part_meta_len > 0 {
            let meta_start = idx.part_meta_start as usize;
            let meta_end = meta_start.saturating_add(idx.part_meta_len as usize);
            if meta_end > bytes.len() {
                return Err("WAL part metadata exceeds payload length".to_string());
            }
            part_meta_blobs.insert(idx.key.clone(), bytes[meta_start..meta_end].to_vec());
        }
        partitions_meta.insert(
            idx.key.clone(),
            (
                idx.bytes,
                SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(idx.updated_at_secs),
            ),
        );
        batches.insert(idx.key.clone(), payload);
    }
    if !mutated {
        return Ok(None);
    }
    crate::buffer::segment_file::encode_snapshot(
        &meta.offsets,
        &batches,
        &partitions_meta,
        &part_meta_blobs,
        SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(meta.created_at_secs),
    )
    .map(Some)
    .map_err(|err| err.to_string())
}

fn persist_rewritten_payload(
    config: &crate::helpers::configuration::Config,
    old_location: &skippr_wal_catalog::PayloadLocation,
    segment_id: &str,
    encoded: crate::buffer::segment_file::EncodedSnapshot,
) -> Result<(), String> {
    let handle = crate::buffer::wal_store::ingest_wal_handle()
        .ok_or_else(|| "WAL rewrite requires an installed catalog handle".to_string())?;
    let new_location = match old_location {
        skippr_wal_catalog::PayloadLocation::Disk { path } => {
            let path = std::path::Path::new(path);
            let dir = path
                .parent()
                .ok_or_else(|| "WAL segment has no parent".to_string())?;
            let tmp = SegmentFile::new(dir, &format!("{segment_id}-migrating"))
                .map_err(|err| err.to_string())?;
            crate::buffer::segment_file::write_payload_atomically(&tmp.path, &encoded.bytes)
                .map_err(|err| err.to_string())?;
            crate::helpers::fsync::fsync_dir(dir).map_err(|err| err.to_string())?;
            skippr_wal_catalog::PayloadLocation::Disk {
                path: tmp.path.to_string_lossy().into_owned(),
            }
        }
        skippr_wal_catalog::PayloadLocation::S3 { bucket, key } => {
            let new_key = format!("{key}-migrating");
            let put_bucket = bucket.clone();
            let put_key = new_key.clone();
            let body = encoded.bytes.clone();
            crate::buffer::wal_store::block_on_async(async move {
                crate::helpers::s3::put_bytes(
                    &put_bucket,
                    &put_key,
                    &body,
                    "application/octet-stream",
                )
                .await
                .map_err(|err| std::io::Error::other(err.to_string()))
            })
            .map_err(|err| err.to_string())?;
            skippr_wal_catalog::PayloadLocation::S3 {
                bucket: bucket.clone(),
                key: new_key,
            }
        }
    };
    handle
        .replace_payload_location(
            segment_id,
            new_location.clone(),
            encoded.meta.total_bytes,
            encoded.sha256,
        )
        .map_err(|err| err.to_string())?;
    match old_location {
        skippr_wal_catalog::PayloadLocation::Disk { path } => {
            let old = std::path::Path::new(path);
            if let skippr_wal_catalog::PayloadLocation::Disk { path: ref new_path } = new_location {
                if old != std::path::Path::new(new_path) {
                    if let Err(err) = std::fs::remove_file(old) {
                        if err.kind() != std::io::ErrorKind::NotFound {
                            tracing::warn!(
                                path = %old.display(),
                                error = %err,
                                "WAL rewrite drop of old payload failed; gc_orphan_payloads is the backstop"
                            );
                        }
                    }
                }
                let admitted = SegmentFile::read_index_path(std::path::Path::new(new_path))
                    .map_err(|err| err.to_string())?;
                crate::buffer::ingest_buffer::Buffers::reregister_disk_segment(
                    config,
                    std::path::PathBuf::from(new_path),
                    admitted,
                );
            }
        }
        skippr_wal_catalog::PayloadLocation::S3 { bucket, key } => {
            let bucket = bucket.clone();
            let key = key.clone();
            if let Err(err) = crate::buffer::wal_store::block_on_async({
                let bucket = bucket.clone();
                let key = key.clone();
                async move {
                    crate::helpers::s3::delete_object(&bucket, &key)
                        .await
                        .map_err(|err| std::io::Error::other(err.to_string()))
                }
            }) {
                tracing::warn!(
                    bucket,
                    key,
                    error = %err,
                    "WAL rewrite drop of old S3 payload failed; gc_orphan_payloads is the backstop"
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::segment_file::PartitionKey;
    use arrow::array::Int64Array;
    use arrow::datatypes::DataType;
    use std::collections::HashMap;
    use std::io::{Read, Seek};

    #[test]
    fn rename_rewrites_arrow_name_and_keeps_field_id() {
        let mut meta = HashMap::new();
        meta.insert("PARQUET:field_id".into(), "7".into());
        let field = Field::new("price", DataType::Int64, true).with_metadata(meta);
        let schema = Schema::new(vec![field]);
        let batch = RecordBatch::try_new(
            Arc::new(schema),
            vec![Arc::new(Int64Array::from(vec![1, 2]))],
        )
        .unwrap();
        let rewritten = rewrite_record_batch(
            batch,
            &SchemaAlterOp::Rename {
                from: "price".into(),
                to: "amount".into(),
            },
            7,
        )
        .unwrap();
        assert_eq!(rewritten.schema().field(0).name(), "amount");
        assert_eq!(
            rewritten
                .schema()
                .field(0)
                .metadata()
                .get("PARQUET:field_id"),
            Some(&"7".to_string())
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn rewrite_segment_persists_renamed_field() {
        let dir = std::env::temp_dir().join(format!("skippr_mig_wal_{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let pipeline_key = skippr_lease::PipelineKey::new("t", "w", "p").unwrap();
        let paths = skippr_lease::PipelinePaths::legacy_disk(&dir);
        let clock = Arc::new(skippr_lease::SystemClock::new());
        let guard = skippr_lease::LeaseGuard::single_node(pipeline_key.clone(), clock);
        let fence = crate::buffer::wal_handle::catalog_fence_from_guard(&guard).unwrap();
        let catalog = crate::buffer::wal_handle::memory_catalog_for_test(fence);
        let db = sled::open(dir.join("offsets")).unwrap();
        let tree = db.open_tree("offsets").unwrap();
        let offsets: Arc<dyn crate::helpers::offset_store::OffsetStore> =
            Arc::new(crate::helpers::offset_store::SledOffsetStore::new(tree));
        let handle = crate::buffer::wal_handle::PipelineWalHandle::new(
            pipeline_key.clone(),
            paths,
            guard,
            catalog,
            offsets,
        );
        crate::buffer::wal_handle::install_wal_handle(handle.clone());
        let mut field_meta = HashMap::new();
        field_meta.insert("PARQUET:field_id".into(), "7".into());
        let field = Field::new("price", DataType::Int64, true).with_metadata(field_meta);
        let schema = Schema::new(vec![field]);
        let batch = RecordBatch::try_new(
            Arc::new(schema),
            vec![Arc::new(Int64Array::from(vec![1, 2]))],
        )
        .unwrap();
        let key = PartitionKey {
            sink_ref: "data_outputs.test".into(),
            namespace: "trips".into(),
            partition: String::new(),
            time: Some(0),
            schema_fingerprint: "schema".into(),
        };
        let mut batches = HashMap::new();
        batches.insert(key.clone(), WalPayload::Arrow(vec![batch]));
        handle
            .persist_segment(
                "t1",
                &HashMap::new(),
                &batches,
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
                std::time::SystemTime::UNIX_EPOCH,
            )
            .await
            .unwrap();
        let path = handle.live_disk_payload_paths().unwrap()[0].clone();

        let config = crate::helpers::configuration::Config::new();
        rewrite_segment_file(
            &config,
            &path,
            "t1",
            "trips",
            &SchemaAlterOp::Rename {
                from: "price".into(),
                to: "amount".into(),
            },
            7,
        )
        .unwrap();

        let rewritten_path = handle.live_disk_payload_paths().unwrap()[0].clone();
        let admitted = SegmentFile::read_index_path(&rewritten_path).unwrap();
        let mut file = std::fs::File::open(&rewritten_path).unwrap();
        file.seek(std::io::SeekFrom::Start(admitted.index[0].start))
            .unwrap();
        let mut take = (&mut file).take(admitted.index[0].len);
        let reader = StreamReader::try_new(&mut take, None).unwrap();
        let rewritten = reader.into_iter().next().unwrap().unwrap();
        assert_eq!(rewritten.schema().field(0).name(), "amount");
        crate::buffer::wal_handle::remove_wal_handle(&pipeline_key);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[serial_test::serial]
    fn rewrite_live_wal_does_not_skip_s3_rows() {
        let src = include_str!("wal.rs");
        assert!(
            !src.contains(concat!("PayloadLocation::S3 { .. } => con", "tinue")),
            "S3 live rows must be rewritten, not skipped"
        );
        let rewrite = src.find("fn rewrite_wal_bytes").expect("rewrite_wal_bytes");
        let persist = src
            .find("fn persist_rewritten_payload")
            .expect("persist_rewritten_payload");
        let rewrite_body = &src[rewrite..persist];
        assert!(
            !rewrite_body.contains("SystemTime::now()"),
            "migration encode must use snapshot created_at, not wall clock"
        );
        assert!(rewrite_body.contains("meta.created_at_secs"));
        assert!(rewrite_body.contains("idx.updated_at_secs"));
        let persist_end = src[persist..]
            .find("#[cfg(test)]")
            .map(|off| persist + off)
            .unwrap_or(src.len());
        let persist_body = &src[persist..persist_end];
        assert!(persist_body.contains("write_payload_atomically"));
        assert!(!persist_body.contains("write_path_sync"));
        assert!(persist_body.contains("tracing::warn!"));
        let dir = std::env::temp_dir().join(format!("skippr_mig_s3_{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let pipeline_key = skippr_lease::PipelineKey::new("t", "w", "p").unwrap();
        let paths = skippr_lease::PipelinePaths::legacy_disk(&dir);
        let clock = Arc::new(skippr_lease::SystemClock::new());
        let guard = skippr_lease::LeaseGuard::single_node(pipeline_key.clone(), clock);
        let fence = crate::buffer::wal_handle::catalog_fence_from_guard(&guard).unwrap();
        let catalog = crate::buffer::wal_handle::memory_catalog_for_test(fence);
        let db = sled::open(dir.join("offsets")).unwrap();
        let tree = db.open_tree("offsets").unwrap();
        let offsets: Arc<dyn crate::helpers::offset_store::OffsetStore> =
            Arc::new(crate::helpers::offset_store::SledOffsetStore::new(tree));
        let handle = crate::buffer::wal_handle::PipelineWalHandle::new(
            pipeline_key.clone(),
            paths,
            guard,
            catalog,
            offsets,
        );
        crate::buffer::wal_handle::install_wal_handle(handle.clone());
        let seq = skippr_wal_catalog::CatalogSeq::new(1);
        let row = skippr_wal_catalog::SegmentRow {
            segment_id: skippr_lease::SegmentId::new("s3live").unwrap(),
            seq,
            location: skippr_wal_catalog::PayloadLocation::S3 {
                bucket: "wal-bucket".into(),
                key: "wal/s3live.seg".into(),
            },
            payload_len: 1,
            payload_sha256: [0u8; 32],
            slice_count: 1,
            done: skippr_wal_catalog::SliceBitmap::new(1),
            offsets: Vec::new(),
            checkpoints: Vec::new(),
            epoch: fence.epoch,
            version: 0,
        };
        let mut txn = skippr_wal_catalog::CatalogTxn::new();
        txn.put(
            skippr_wal_catalog::CatalogKey::Segment(row.segment_id.clone()),
            skippr_wal_catalog::CatalogRow::Segment(row),
            skippr_wal_catalog::PutCond::NotExists,
        )
        .unwrap();
        handle.catalog().commit(&txn, fence).unwrap();
        let config = crate::helpers::configuration::Config::new();
        let err = rewrite_live_wal(
            &config,
            "trips",
            &SchemaAlterOp::Rename {
                from: "price".into(),
                to: "amount".into(),
            },
            7,
        )
        .expect_err("S3 rewrite must fail closed, not skip");
        crate::buffer::wal_handle::remove_wal_handle(&pipeline_key);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!err.is_empty(), "{err}");
    }
}
