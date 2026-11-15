//! Rewrite live WAL payloads onto the next output schema. Runs under the
//! schema gate after the WAL writer flushed, so catalog rows are the whole WAL.

use crate::buffer::segment_file::{
    is_copy_codec, CopyPart, EncodedSnapshot, SegmentFile, WalPayload, PART_CODEC_ARROW,
};
use crate::buffer::wal_handle::{payload_bytes, PipelineWalHandle};
use crate::discover::schema_alter::{apply_alter, CanonicalAlter};
use crate::helpers::configuration::Config;
use crate::migration::MigrationError;
use arrow::ipc::reader::StreamReader;
use skippr_wal_catalog::{PayloadLocation, SegmentRow};
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, SystemTime};

/// Rewrite every live payload that carries `namespace`. Each rewritten payload
/// lands at a new content-addressed location; the catalog swap is atomic and
/// the old payload is left for `gc_orphan_payloads`.
pub(super) fn rewrite_live_wal(
    config: &Config,
    handle: &PipelineWalHandle,
    namespace: &str,
    alter: &CanonicalAlter,
    seq: u64,
) -> Result<(), MigrationError> {
    for row in handle.live_segment_rows()? {
        let bytes = payload_bytes(&row.location, &row.payload_sha256)?;
        let Some(encoded) = rewrite_wal_bytes(&bytes, namespace, alter)? else {
            continue;
        };
        let location = write_alter_payload(&row, &encoded, seq)?;
        handle.replace_payload(
            row.segment_id.as_str(),
            location.clone(),
            &encoded.meta,
            encoded.sha256,
        )?;
        crate::buffer::ingest_buffer::Buffers::register_catalog_row(
            handle,
            config,
            &SegmentRow { location, ..row },
        )?;
    }
    Ok(())
}

fn rewrite_wal_bytes(
    bytes: &[u8],
    namespace: &str,
    alter: &CanonicalAlter,
) -> Result<Option<EncodedSnapshot>, MigrationError> {
    let meta = SegmentFile::read_metadata_from_bytes(bytes)?;
    let mut batches: HashMap<_, WalPayload> = HashMap::new();
    let mut partitions_meta = HashMap::new();
    let mut part_meta_blobs = HashMap::new();
    let mut mutated = false;
    for idx in &meta.index {
        if is_copy_codec(idx.codec) && idx.key.namespace == namespace {
            return Err(MigrationError::CopyPayload);
        }
        let start = idx.start as usize;
        let slice = bytes
            .get(start..start.saturating_add(idx.len as usize))
            .ok_or(MigrationError::TruncatedPayload)?;
        let payload = if idx.codec == PART_CODEC_ARROW {
            let reader = StreamReader::try_new(std::io::Cursor::new(slice), None)?;
            let mut arrow_batches = Vec::new();
            for batch in reader {
                let batch = batch?;
                if idx.key.namespace != namespace {
                    arrow_batches.push(batch);
                    continue;
                }
                let before = batch.schema();
                let after = apply_alter(batch, alter)?;
                mutated |= after.schema() != before;
                arrow_batches.push(after);
            }
            WalPayload::Arrow(arrow_batches)
        } else {
            let records = crate::buffer::segment_file::decode_copy_part(idx.codec, slice)?;
            WalPayload::Copy(CopyPart::encode(&records)?)
        };
        if idx.part_meta_len > 0 {
            let start = idx.part_meta_start as usize;
            let blob = bytes
                .get(start..start.saturating_add(idx.part_meta_len as usize))
                .ok_or(MigrationError::TruncatedPayload)?;
            part_meta_blobs.insert(idx.key.clone(), blob.to_vec());
        }
        partitions_meta.insert(
            idx.key.clone(),
            (
                idx.bytes,
                SystemTime::UNIX_EPOCH + Duration::from_secs(idx.updated_at_secs),
            ),
        );
        batches.insert(idx.key.clone(), payload);
    }
    if !mutated {
        return Ok(None);
    }
    Ok(Some(crate::buffer::segment_file::encode_snapshot(
        &meta.offsets,
        &batches,
        &partitions_meta,
        &part_meta_blobs,
        SystemTime::UNIX_EPOCH + Duration::from_secs(meta.created_at_secs),
    )?))
}

/// `…/segs/alter-{seq}/{segment}.seg` beside the current payload: the file stem
/// stays the segment id and identical bytes land on the same path.
fn write_alter_payload(
    row: &SegmentRow,
    encoded: &EncodedSnapshot,
    seq: u64,
) -> Result<PayloadLocation, MigrationError> {
    let dir = format!("alter-{seq}");
    let file = format!("{}.seg", row.segment_id.as_str());
    match &row.location {
        PayloadLocation::Disk { path } => {
            let parent = Path::new(path)
                .parent()
                .ok_or(MigrationError::TruncatedPayload)?;
            let segs = match parent.file_name().and_then(|name| name.to_str()) {
                Some(name) if name.starts_with("alter-") => parent.parent().unwrap_or(parent),
                _ => parent,
            };
            let dir = segs.join(dir);
            std::fs::create_dir_all(&dir)?;
            let target = dir.join(file);
            crate::buffer::segment_file::write_payload_atomically(&target, &encoded.bytes)?;
            crate::helpers::fsync::fsync_dir(&dir)?;
            crate::helpers::fsync::fsync_dir(segs)?;
            Ok(PayloadLocation::Disk {
                path: target.to_string_lossy().into_owned(),
            })
        }
        PayloadLocation::S3 { bucket, key } => {
            let prefix = key.rsplit_once('/').map_or("", |(prefix, _)| prefix);
            let prefix = match prefix.rsplit_once('/') {
                Some((up, last)) if last.starts_with("alter-") => up,
                None if prefix.starts_with("alter-") => "",
                _ => prefix,
            };
            let new_key = [prefix, dir.as_str(), file.as_str()]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("/");
            let (put_bucket, put_key, body) =
                (bucket.clone(), new_key.clone(), encoded.bytes.clone());
            crate::buffer::wal_store::block_on_async(async move {
                crate::helpers::s3::put_bytes(
                    &put_bucket,
                    &put_key,
                    &body,
                    "application/octet-stream",
                )
                .await
                .map_err(|err| std::io::Error::other(err.to_string()))
            })?;
            Ok(PayloadLocation::S3 {
                bucket: bucket.clone(),
                key: new_key,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::segment_file::PartitionKey;
    use crate::discover::schema_alter::{AlterColumn, FlattenMode, OutputName};
    use crate::discover::SkipprDataType;
    use arrow::array::{Int64Array, RecordBatch};
    use arrow::datatypes::{DataType, Field, Schema};
    use std::io::{Read, Seek};
    use std::sync::Arc;

    fn rename_price() -> CanonicalAlter {
        CanonicalAlter::Rename {
            column: AlterColumn {
                name: OutputName::parse_sql(FlattenMode::Nested, "price").unwrap(),
                id: 7,
                ty: SkipprDataType::Long,
            },
            to_leaf: "amount".into(),
        }
    }

    fn first_field_name(path: &Path) -> String {
        let admitted = SegmentFile::read_index_path(path).unwrap();
        let mut file = std::fs::File::open(path).unwrap();
        file.seek(std::io::SeekFrom::Start(admitted.index[0].start))
            .unwrap();
        let take = (&mut file).take(admitted.index[0].len);
        let reader = StreamReader::try_new(take, None).unwrap();
        let batch = reader.into_iter().next().unwrap().unwrap();
        batch.schema().field(0).name().to_string()
    }

    async fn disk_handle(dir: &Path) -> Arc<PipelineWalHandle> {
        let fence = skippr_wal_catalog::CatalogFence::local();
        let handle = PipelineWalHandle::new(
            skippr_lease::PipelineKey::new("t", "w", "p").unwrap(),
            skippr_lease::PipelinePaths::legacy_disk(dir),
            crate::buffer::wal_handle::memory_catalog_for_test(fence),
            crate::helpers::offset_store::memory_offset_store(),
            fence,
        );
        let batch = RecordBatch::try_new(
            Arc::new(Schema::new(vec![Field::new(
                "price",
                DataType::Int64,
                true,
            )])),
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
        handle
            .persist_segment(
                "t1",
                &HashMap::new(),
                &HashMap::from([(key, WalPayload::Arrow(vec![batch]))]),
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
                SystemTime::UNIX_EPOCH,
            )
            .await
            .unwrap();
        handle
    }

    #[test]
    fn apply_pending_installs_disk_wal_handle_before_rewrite() {
        let src = include_str!("apply.rs");
        assert!(
            src.contains("ensure_disk_wal_handle"),
            "migrate CLI must open leftover disk WAL before rewrite_live_wal"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn rewrite_keeps_segment_id_and_swaps_catalog_rows() {
        let dir = tempfile::tempdir().unwrap();
        let handle = disk_handle(dir.path()).await;
        let before = handle.live_disk_payload_paths().unwrap()[0].clone();
        let config = Config::new();

        rewrite_live_wal(&config, &handle, "trips", &rename_price(), 0).unwrap();

        let after = handle.live_disk_payload_paths().unwrap()[0].clone();
        assert_ne!(after, before);
        assert_eq!(after.file_stem().unwrap(), "t1");
        assert_eq!(
            after
                .parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy(),
            "alter-0"
        );
        assert_eq!(first_field_name(&after), "amount");
        let row = handle.live_segment_rows().unwrap().remove(0);
        let catalog_index = handle.load_or_build_index_meta(&row).unwrap().index;
        let file_index = SegmentFile::read_index_path(&after).unwrap().index;
        assert_eq!(
            catalog_index
                .iter()
                .map(|idx| (idx.start, idx.len))
                .collect::<Vec<_>>(),
            file_index
                .iter()
                .map(|idx| (idx.start, idx.len))
                .collect::<Vec<_>>()
        );

        rewrite_live_wal(&config, &handle, "trips", &rename_price(), 0).unwrap();
        assert_eq!(handle.live_disk_payload_paths().unwrap()[0], after);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn unreadable_payload_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let handle = disk_handle(dir.path()).await;
        std::fs::remove_file(&handle.live_disk_payload_paths().unwrap()[0]).unwrap();
        assert!(matches!(
            rewrite_live_wal(&Config::new(), &handle, "trips", &rename_price(), 0),
            Err(MigrationError::Durable(_))
        ));
    }
}
