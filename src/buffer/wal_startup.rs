use std::sync::Arc;

use skippr_lease::DurableError;
use skippr_wal_catalog::{CatalogRow, FormatPhase, FormatRow};

use crate::buffer::wal_catalog_import::{gc_orphan_payloads, import_if_needed};
use crate::buffer::wal_handle::PipelineWalHandle;
use crate::helpers::configuration::Config;

pub async fn wal_startup(
    handle: &Arc<PipelineWalHandle>,
    config: &Config,
    gc_orphans: bool,
) -> Result<(), DurableError> {
    let started = std::time::Instant::now();
    let _ = handle.guard().require_recovery_epoch()?;
    import_if_needed(handle, config)?;
    crate::buffer::ingest_buffer::Buffers::hydrate_from_catalog(handle, config);
    let acked = handle.apply_acked_in_memory()?;
    tracing::info!(compactions = acked.len(), "WAL catalog redo_acked begin");
    handle.republish_all().await?;
    if gc_orphans {
        let clean = matches!(
            handle
                .catalog()
                .get(&skippr_wal_catalog::CatalogKey::Format)
                .map_err(|err| DurableError::Io(err.to_string()))?,
            Some(CatalogRow::Format(FormatRow {
                phase: FormatPhase::Clean,
                ..
            }))
        );
        if clean {
            gc_orphan_payloads(handle, config)?;
        }
    }
    handle.spawn_durable_redo(acked);
    tracing::info!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        "WAL startup complete"
    );
    Ok(())
}

pub fn wal_attach_replica(handle: &PipelineWalHandle) -> Result<bool, DurableError> {
    handle.payload_ready()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::compaction_transaction::{
        CompactionTransaction, SegmentSourceDescriptor, SinkWriteSemantics, WalPartRef,
    };
    use crate::buffer::segment_file::{PartitionKey, WalPayload};
    use crate::buffer::wal_handle::memory_catalog_for_test;
    use crate::helpers::offset_store::SledOffsetStore;
    use crate::plugins::source_contract::WritePolicy;
    use arrow::array::Int32Array;
    use arrow::record_batch::RecordBatch;
    use arrow_schema::{DataType, Field, Schema};
    use serial_test::serial;
    use skippr_lease::{LeaseGuard, PipelineKey, PipelinePaths, SegmentId, SystemClock};
    use skippr_wal_catalog::{CatalogKey, CatalogRow, CompactionId, CompactionState};
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::SystemTime;

    fn test_handle(dir: &std::path::Path) -> Arc<PipelineWalHandle> {
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::legacy_disk(dir);
        let guard = LeaseGuard::single_node(key.clone(), Arc::new(SystemClock::new()));
        let fence = crate::buffer::wal_handle::catalog_fence_from_guard(&guard).unwrap();
        let catalog = memory_catalog_for_test(fence);
        let db = sled::open(dir.join("offsets")).unwrap();
        let offsets: Arc<dyn crate::helpers::offset_store::OffsetStore> =
            Arc::new(SledOffsetStore::new(db.open_tree("o").unwrap()));
        PipelineWalHandle::new(key, paths, guard, catalog, offsets)
    }

    fn one_batch() -> (
        HashMap<PartitionKey, WalPayload>,
        HashMap<PartitionKey, (u64, SystemTime)>,
    ) {
        let key = PartitionKey {
            sink_ref: "out".into(),
            namespace: "ns".into(),
            partition: String::new(),
            time: Some(0),
            schema_fingerprint: "schema".into(),
        };
        let schema = Schema::new(vec![Field::new("v", DataType::Int32, false)]);
        let batch = RecordBatch::try_new(
            Arc::new(schema),
            vec![Arc::new(Int32Array::from(vec![1, 2, 3]))],
        )
        .unwrap();
        let mut batches = HashMap::new();
        batches.insert(key.clone(), WalPayload::Arrow(vec![batch]));
        let mut parts_meta = HashMap::new();
        parts_meta.insert(key, (0, SystemTime::now()));
        (batches, parts_meta)
    }

    #[tokio::test]
    #[serial]
    async fn wal_startup_redoes_acked_compaction() {
        let dir = tempfile::tempdir().unwrap();
        let handle = test_handle(dir.path());
        let (batches, parts_meta) = one_batch();
        let result = handle
            .persist_segment(
                "acked-seg",
                &HashMap::new(),
                &batches,
                &parts_meta,
                &HashMap::new(),
                &HashMap::new(),
            )
            .await
            .unwrap();
        let idx = &result.meta.index[0];
        let txn = CompactionTransaction::new(
            "out".into(),
            "ns".into(),
            "schema".into(),
            WritePolicy::Append,
            SinkWriteSemantics::AtLeastOnce,
            vec![WalPartRef {
                segment_id: "acked-seg".into(),
                source: SegmentSourceDescriptor::Disk {
                    path: handle.paths().segs.join("acked-seg.seg"),
                },
                start: idx.start,
                len: idx.len,
                key: idx.key.clone(),
                cdc_meta_hash: None,
                ordinal: Some(idx.slice_ordinal),
            }],
            "out.parquet".into(),
        )
        .mark_acked();
        handle
            .put_compaction(
                &txn.id,
                CompactionState::Acked,
                serde_json::to_vec(&txn).unwrap(),
            )
            .await
            .unwrap();
        wal_startup(&handle, &Config::new(), true).await.unwrap();
        assert!(handle
            .slice_is_done("acked-seg", idx.slice_ordinal)
            .unwrap());
        handle
            .redo_acked_durable_batches(&[txn.id.clone()])
            .await
            .unwrap();
        assert!(handle
            .catalog()
            .get(&CatalogKey::Compaction(CompactionId::new(txn.id).unwrap()))
            .unwrap()
            .is_none());
        assert!(handle
            .catalog()
            .get(&CatalogKey::Segment(SegmentId::new("acked-seg").unwrap()))
            .unwrap()
            .is_none());
        assert!(!handle.paths().segs.join("acked-seg.seg").exists());
    }

    fn capture_logs(f: impl FnOnce()) -> String {
        use std::io::Write;
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone, Default)]
        struct Capture(Arc<Mutex<Vec<u8>>>);
        impl Write for Capture {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().expect("capture").extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> MakeWriter<'a> for Capture {
            type Writer = Capture;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let capture = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .with_writer(capture.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        let bytes = capture.0.lock().expect("capture").clone();
        String::from_utf8(bytes).unwrap()
    }

    fn sent_txn(namespace: &str) -> CompactionTransaction {
        CompactionTransaction::new(
            "out".into(),
            namespace.into(),
            "schema".into(),
            WritePolicy::Append,
            SinkWriteSemantics::AtLeastOnce,
            Vec::new(),
            format!("{namespace}.parquet"),
        )
        .mark_sent()
    }

    #[test]
    #[serial]
    fn wal_startup_logs_sent_retries_as_count_not_per_id() {
        let dir = tempfile::tempdir().unwrap();
        let handle = test_handle(dir.path());
        let (batches, parts_meta) = one_batch();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .block_on(handle.persist_segment(
                "log-seg",
                &HashMap::new(),
                &batches,
                &parts_meta,
                &HashMap::new(),
                &HashMap::new(),
            ))
            .unwrap();
        let mut ids = Vec::new();
        for ns in ["ns-a", "ns-b", "ns-c"] {
            let txn = sent_txn(ns);
            ids.push(txn.id.clone());
            runtime
                .block_on(handle.put_compaction(
                    &txn.id,
                    CompactionState::Sent,
                    serde_json::to_vec(&txn).unwrap(),
                ))
                .unwrap();
        }
        let logged = capture_logs(|| {
            runtime.block_on(async {
                wal_startup(&handle, &Config::new(), true).await.unwrap();
            });
        });
        assert!(
            logged.contains("WAL catalog hydrate begin"),
            "hydrate must log work size before hashing: {logged}"
        );
        assert!(logged.contains("WAL catalog hydrate complete"), "{logged}");
        assert!(
            logged.contains("sent_retried=3"),
            "Sent retries must be a count: {logged}"
        );
        assert_eq!(
            logged.matches("retrying sent compaction").count(),
            0,
            "must not warn once per Sent id: {logged}"
        );
        for id in &ids {
            assert!(
                !logged.contains(id),
                "startup logs must not dump compaction ids ({id}): {logged}"
            );
        }
        assert!(
            logged.contains("WAL catalog redo_acked begin")
                || logged.contains("WAL catalog redo_acked complete"),
            "{logged}"
        );
        assert!(
            logged.contains("WAL catalog republish complete"),
            "{logged}"
        );
        assert!(logged.contains("WAL startup complete"), "{logged}");
        assert!(
            logged.len() < 8_000,
            "startup logs grew to {} bytes",
            logged.len()
        );
    }

    #[test]
    fn wal_startup_log_lines_are_counts_and_progress() {
        let hydrate = include_str!("ingest_buffer.rs");
        let redo = include_str!("wal_handle.rs");
        let import = include_str!("wal_catalog_import.rs");
        let startup = include_str!("wal_startup.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production wal_startup");
        assert!(!hydrate.contains("retrying sent compaction manifest after process start"));
        assert!(hydrate.contains("WAL catalog hydrate begin"));
        assert!(hydrate.contains("WAL catalog hydrate progress"));
        assert!(hydrate.contains("WAL catalog hydrate complete"));
        assert!(hydrate.contains("WAL_CATALOG_PROGRESS_EVERY"));
        assert!(redo.contains("WAL catalog redo_acked begin"));
        assert!(redo.contains("WAL catalog redo_acked progress"));
        assert!(redo.contains("WAL catalog redo_acked complete"));
        assert!(redo.contains("WAL catalog republish begin"));
        assert!(redo.contains("WAL catalog republish complete"));
        assert!(!redo.contains("acked compaction redo deferred; catalog row kept"));
        assert!(import.contains("WAL_CATALOG_PROGRESS_EVERY"));
        assert!(startup.contains("WAL startup complete"));
        assert!(startup.contains("apply_acked_in_memory"));
        assert!(startup.contains("spawn_durable_redo"));
        assert!(!startup.contains("redo_acked_compactions().await"));
        assert!(!startup.contains("redo_acked_durable_batches("));
    }

    #[tokio::test]
    #[serial]
    async fn wal_startup_hydrate_keeps_live_segment_row() {
        let dir = tempfile::tempdir().unwrap();
        let handle = test_handle(dir.path());
        let (batches, parts_meta) = one_batch();
        handle
            .persist_segment(
                "live-seg",
                &HashMap::new(),
                &batches,
                &parts_meta,
                &HashMap::new(),
                &HashMap::new(),
            )
            .await
            .unwrap();
        wal_startup(&handle, &Config::new(), true).await.unwrap();
        match handle
            .catalog()
            .get(&CatalogKey::Segment(SegmentId::new("live-seg").unwrap()))
            .unwrap()
        {
            Some(CatalogRow::Segment(row)) => {
                assert_eq!(row.segment_id.as_str(), "live-seg");
            }
            other => panic!("expected live segment row, got {other:?}"),
        }
        assert!(handle.paths().segs.join("live-seg.seg").exists());
    }
}
