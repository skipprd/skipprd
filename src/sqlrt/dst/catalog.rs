use skippr_lease::{LeaseEpoch, NodeId};
use skippr_wal_catalog::{
    CatalogError, CatalogFence, MemoryWalCatalog, WalCatalogRead, WalCatalogStore,
};

fn fence() -> CatalogFence {
    CatalogFence {
        owner: NodeId::generate(),
        epoch: LeaseEpoch::new(1),
    }
}

#[test]
fn c_putcond_not_exists_same_sha() {
    use skippr_wal_catalog::{
        CatalogError, CatalogKey, CatalogRow, CatalogTxn, PutCond, WalCatalogStore,
    };
    let store = MemoryWalCatalog::new();
    let f = fence();
    store.install_lease(f.owner, f.epoch);
    let key = CatalogKey::Segment(skippr_lease::SegmentId::new("seg-sha-once").unwrap());
    let row = occ_segment("seg-sha-once", 0);
    let mut create = CatalogTxn::new();
    create
        .put(
            key.clone(),
            CatalogRow::Segment(row.clone()),
            PutCond::NotExists,
        )
        .unwrap();
    store.commit(&create, f).unwrap();
    let mut again = CatalogTxn::new();
    again
        .put(
            key.clone(),
            CatalogRow::Segment(row.clone()),
            PutCond::NotExists,
        )
        .unwrap();
    assert_eq!(
        store.commit(&again, f).unwrap_err(),
        CatalogError::ConditionFailed
    );
    let mut same = CatalogTxn::new();
    same.put(key, CatalogRow::Segment(row), PutCond::SameSha)
        .unwrap();
    store.commit(&same, f).unwrap();
}

#[test]
fn c_stale_fence_rejected() {
    let store = MemoryWalCatalog::new();
    let f = fence();
    store.install_lease(f.owner, f.epoch);
    store.release_lease();
    let mut txn = skippr_wal_catalog::CatalogTxn::new();
    txn.put(
        skippr_wal_catalog::CatalogKey::Format,
        skippr_wal_catalog::CatalogRow::Format(skippr_wal_catalog::FormatRow {
            version: skippr_wal_catalog::FORMAT_VERSION,
            phase: skippr_wal_catalog::FormatPhase::Imported,
            row_version: 0,
        }),
        skippr_wal_catalog::PutCond::NotExists,
    )
    .unwrap();
    assert_eq!(store.commit(&txn, f).unwrap_err(), CatalogError::Fenced);
}

#[test]
fn c_unreadable_compaction_json_fail_closed() {
    assert!(
        serde_json::from_slice::<crate::buffer::compaction_transaction::CompactionTransaction>(&[
            0xff, 0x00, 0x01
        ])
        .is_err()
    );
}

#[test]
fn c_occ_concurrent_reclaim() {
    use skippr_wal_catalog::{
        CatalogError, CatalogKey, CatalogRow, CatalogTxn, PutCond, WalCatalogStore,
    };
    let store = MemoryWalCatalog::new();
    let f = fence();
    store.install_lease(f.owner, f.epoch);
    let key = CatalogKey::Segment(skippr_lease::SegmentId::new("seg-occ").unwrap());
    let mut create = CatalogTxn::new();
    create
        .put(
            key.clone(),
            CatalogRow::Segment(occ_segment("seg-occ", 0)),
            PutCond::NotExists,
        )
        .unwrap();
    store.commit(&create, f).unwrap();
    let mut first = CatalogTxn::new();
    first
        .put(
            key.clone(),
            CatalogRow::Segment(occ_segment("seg-occ", 1)),
            PutCond::Version(0),
        )
        .unwrap();
    store.commit(&first, f).unwrap();
    let mut raced = CatalogTxn::new();
    raced
        .put(
            key,
            CatalogRow::Segment(occ_segment("seg-occ", 1)),
            PutCond::Version(0),
        )
        .unwrap();
    assert_eq!(
        store.commit(&raced, f).unwrap_err(),
        CatalogError::ConditionFailed
    );
}

#[test]
fn c_pagination_and_delete_prefix() {
    use skippr_wal_catalog::{
        query_all, CatalogKey, CatalogPrefix, CatalogTxn, CompactionId, CompactionRow,
        CompactionState, PutCond,
    };
    let store = MemoryWalCatalog::new();
    let f = fence();
    store.install_lease(f.owner, f.epoch);
    for i in 0..12u32 {
        let id = CompactionId::new(format!("cmp-{i:02}")).unwrap();
        let mut txn = CatalogTxn::new();
        txn.put(
            CatalogKey::Compaction(id.clone()),
            skippr_wal_catalog::CatalogRow::Compaction(CompactionRow {
                id,
                state: CompactionState::Pending,
                body: vec![i as u8],
                version: 0,
            }),
            PutCond::NotExists,
        )
        .unwrap();
        store.commit(&txn, f).unwrap();
    }
    let page = store
        .query_prefix(CatalogPrefix::Compaction, None, 5)
        .unwrap();
    assert_eq!(page.len(), 5);
    store.delete_prefix(CatalogPrefix::Compaction, f).unwrap();
    assert!(query_all(&store, CatalogPrefix::Compaction)
        .unwrap()
        .is_empty());
}

#[test]
fn c_legacy_slice_put_rejected() {
    use skippr_lease::SegmentId;
    use skippr_wal_catalog::{CatalogError, CatalogKey, CatalogTxn, LegacySliceRow, PutCond};
    let store = MemoryWalCatalog::new();
    let f = fence();
    store.install_lease(f.owner, f.epoch);
    let key = CatalogKey::LegacySlice {
        segment_id: SegmentId::new("legacy-seg").unwrap(),
        ordinal: 0,
    };
    let mut txn = CatalogTxn::new();
    txn.put(
        key,
        skippr_wal_catalog::CatalogRow::LegacySlice(LegacySliceRow {
            segment_id: SegmentId::new("legacy-seg").unwrap(),
            ordinal: 0,
            namespace: "ns".into(),
            partition: "p".into(),
            start: 0,
            len: 1,
            codec: 0,
            part_meta_start: 0,
            part_meta_len: 0,
            version: 0,
        }),
        PutCond::NotExists,
    )
    .unwrap();
    assert_eq!(
        store.commit(&txn, f).unwrap_err(),
        CatalogError::LegacyRowWrite
    );
}

#[test]
fn c_import_leftover_once() {
    use crate::buffer::wal_handle::{catalog_fence_from_guard, memory_catalog_for_test};
    use crate::helpers::offset_store::SledOffsetStore;
    use skippr_lease::{LeaseGuard, PipelineKey, PipelinePaths, SystemClock};
    use skippr_wal_catalog::{CatalogKey, FormatPhase};
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let key = PipelineKey::new("t", "w", "p").unwrap();
    let paths = PipelinePaths::legacy_disk(dir.path());
    let guard = LeaseGuard::single_node(key.clone(), Arc::new(SystemClock::new()));
    let fence = catalog_fence_from_guard(&guard).unwrap();
    let catalog = memory_catalog_for_test(fence);
    let db = sled::open(dir.path().join("offsets")).unwrap();
    let offsets: Arc<dyn crate::helpers::offset_store::OffsetStore> =
        Arc::new(SledOffsetStore::new(db.open_tree("o").unwrap()));
    let handle =
        crate::buffer::wal_handle::PipelineWalHandle::new(key, paths, guard, catalog, offsets);
    std::fs::create_dir_all(&handle.paths().durable).unwrap();
    std::fs::write(handle.paths().durable.join("mutation.log"), b"legacy").unwrap();
    crate::buffer::wal_catalog_import::import_if_needed(
        &handle,
        &crate::helpers::configuration::Config::new(),
    )
    .unwrap();
    crate::buffer::wal_catalog_import::import_if_needed(
        &handle,
        &crate::helpers::configuration::Config::new(),
    )
    .unwrap();
    match handle.catalog().get(&CatalogKey::Format).unwrap() {
        Some(skippr_wal_catalog::CatalogRow::Format(row)) => {
            assert_eq!(row.phase, FormatPhase::Clean)
        }
        other => panic!("expected Clean format, got {other:?}"),
    }
    assert!(!handle.paths().durable.join("mutation.log").exists());
}

#[test]
fn c_unreadable_compaction_json_names_file() {
    let err =
        serde_json::from_slice::<crate::buffer::compaction_transaction::CompactionTransaction>(
            b"not-json",
        )
        .unwrap_err();
    let named =
        crate::buffer::wal_catalog_import::unreadable_compaction_json_error("broken.json", err);
    assert!(
        named
            .to_string()
            .contains("unreadable compaction json broken.json"),
        "{named}"
    );
}

fn occ_segment(id: &str, version: u64) -> skippr_wal_catalog::SegmentRow {
    skippr_wal_catalog::SegmentRow {
        segment_id: skippr_lease::SegmentId::new(id).unwrap(),
        seq: skippr_wal_catalog::CatalogSeq::new(1),
        location: skippr_wal_catalog::PayloadLocation::Disk {
            path: format!("/tmp/{id}.seg"),
        },
        payload_len: 1,
        payload_sha256: [7; 32],
        slice_count: 1,
        done: skippr_wal_catalog::SliceBitmap::new(1),
        offsets: vec![],
        checkpoints: vec![],
        epoch: skippr_lease::LeaseEpoch::new(1),
        version,
    }
}

#[test]
fn c_putcond_version_mismatch() {
    use skippr_wal_catalog::{
        CatalogError, CatalogKey, CatalogRow, CatalogTxn, PutCond, WalCatalogStore,
    };
    let store = MemoryWalCatalog::new();
    let f = fence();
    store.install_lease(f.owner, f.epoch);
    let key = CatalogKey::Segment(skippr_lease::SegmentId::new("seg-ver").unwrap());
    let mut create = CatalogTxn::new();
    create
        .put(
            key.clone(),
            CatalogRow::Segment(occ_segment("seg-ver", 0)),
            PutCond::NotExists,
        )
        .unwrap();
    store.commit(&create, f).unwrap();
    let mut stale = occ_segment("seg-ver", 1);
    stale.version = 1;
    let mut txn = CatalogTxn::new();
    txn.put(key, CatalogRow::Segment(stale), PutCond::Version(7))
        .unwrap();
    assert_eq!(
        store.commit(&txn, f).unwrap_err(),
        CatalogError::VersionMustAdvance
    );
}

#[test]
fn c_deletecond_version_mismatch() {
    use skippr_wal_catalog::{
        CatalogError, CatalogKey, CatalogRow, CatalogTxn, DeleteCond, PutCond, WalCatalogStore,
    };
    let store = MemoryWalCatalog::new();
    let f = fence();
    store.install_lease(f.owner, f.epoch);
    let key = CatalogKey::Segment(skippr_lease::SegmentId::new("seg-del").unwrap());
    let mut create = CatalogTxn::new();
    create
        .put(
            key.clone(),
            CatalogRow::Segment(occ_segment("seg-del", 0)),
            PutCond::NotExists,
        )
        .unwrap();
    store.commit(&create, f).unwrap();
    let mut txn = CatalogTxn::new();
    txn.delete(key, DeleteCond::Version(9)).unwrap();
    assert_eq!(
        store.commit(&txn, f).unwrap_err(),
        CatalogError::ConditionFailed
    );
}

#[test]
fn c_format_imported_resume() {
    use crate::buffer::wal_handle::{catalog_fence_from_guard, memory_catalog_for_test};
    use crate::helpers::offset_store::SledOffsetStore;
    use skippr_lease::{LeaseGuard, PipelineKey, PipelinePaths, SystemClock};
    use skippr_wal_catalog::{
        CatalogKey, CatalogRow, CatalogTxn, FormatPhase, FormatRow, PutCond, FORMAT_VERSION,
    };
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let key = PipelineKey::new("t", "w", "imported").unwrap();
    let paths = PipelinePaths::legacy_disk(dir.path());
    let guard = LeaseGuard::single_node(key.clone(), Arc::new(SystemClock::new()));
    let fence = catalog_fence_from_guard(&guard).unwrap();
    let catalog = memory_catalog_for_test(fence);
    let db = sled::open(dir.path().join("offsets")).unwrap();
    let offsets: Arc<dyn crate::helpers::offset_store::OffsetStore> =
        Arc::new(SledOffsetStore::new(db.open_tree("o").unwrap()));
    let handle =
        crate::buffer::wal_handle::PipelineWalHandle::new(key, paths, guard, catalog, offsets);
    let mut txn = CatalogTxn::new();
    txn.put(
        CatalogKey::Format,
        CatalogRow::Format(FormatRow {
            version: FORMAT_VERSION,
            phase: FormatPhase::Imported,
            row_version: 0,
        }),
        PutCond::NotExists,
    )
    .unwrap();
    handle.catalog().commit(&txn, fence).unwrap();
    crate::buffer::wal_catalog_import::import_if_needed(
        &handle,
        &crate::helpers::configuration::Config::new(),
    )
    .unwrap();
    match handle.catalog().get(&CatalogKey::Format).unwrap() {
        Some(CatalogRow::Format(row)) => assert_eq!(row.phase, FormatPhase::Clean),
        other => panic!("Imported must resume to Clean, got {other:?}"),
    }
}

#[test]
fn c_pending_to_sent() {
    use skippr_wal_catalog::{
        CatalogKey, CatalogRow, CatalogTxn, CompactionId, CompactionRow, CompactionState, PutCond,
        WalCatalogRead, WalCatalogStore,
    };
    let store = MemoryWalCatalog::new();
    let f = fence();
    store.install_lease(f.owner, f.epoch);
    let id = CompactionId::new("c1").unwrap();
    let mut create = CatalogTxn::new();
    create
        .put(
            CatalogKey::Compaction(id.clone()),
            CatalogRow::Compaction(CompactionRow {
                id: id.clone(),
                state: CompactionState::Pending,
                body: vec![],
                version: 0,
            }),
            PutCond::NotExists,
        )
        .unwrap();
    store.commit(&create, f).unwrap();
    let mut sent = CatalogTxn::new();
    sent.put(
        CatalogKey::Compaction(id.clone()),
        CatalogRow::Compaction(CompactionRow {
            id: id.clone(),
            state: CompactionState::Sent,
            body: vec![],
            version: 1,
        }),
        PutCond::Version(0),
    )
    .unwrap();
    store.commit(&sent, f).unwrap();
    match store.get(&CatalogKey::Compaction(id)).unwrap() {
        Some(CatalogRow::Compaction(row)) => assert_eq!(row.state, CompactionState::Sent),
        other => panic!("{other:?}"),
    }
}

#[test]
fn c_sent_vs_acked() {
    use crate::cluster::failpoint::FailpointName;
    use crate::plugins::cdc::MutationKind;
    use crate::sqlrt::dst::{compaction_state, run, Op, UserRow, WalEvent};
    use skippr_wal_catalog::CompactionState;
    let acked = run(&[
        Op::IngestWal(WalEvent {
            row: UserRow {
                id: 1,
                name: "a".into(),
                amount: 1,
            },
            mutation: Some(MutationKind::Insert),
            order: 1,
            seq: 1,
            ordinal: 0,
            row_i: 0,
        }),
        Op::CompactThru(1),
    ]);
    assert_eq!(compaction_state(&acked, 1), Some(CompactionState::Acked));
    let sent = run(&[
        Op::IngestWal(WalEvent {
            row: UserRow {
                id: 1,
                name: "a".into(),
                amount: 1,
            },
            mutation: Some(MutationKind::Insert),
            order: 1,
            seq: 1,
            ordinal: 0,
            row_i: 0,
        }),
        Op::Failpoint(FailpointName::AfterCompactionSink),
        Op::CompactThru(1),
    ]);
    assert_eq!(compaction_state(&sent, 1), Some(CompactionState::Sent));
}

#[test]
fn c_after_compaction_sink_leaves_sent() {
    use crate::cluster::failpoint::FailpointName;
    use crate::plugins::cdc::MutationKind;
    use crate::sqlrt::dst::{compaction_state, run, Op, UserRow, WalEvent};
    use skippr_wal_catalog::CompactionState;
    let world = run(&[
        Op::IngestWal(WalEvent {
            row: UserRow {
                id: 1,
                name: "a".into(),
                amount: 1,
            },
            mutation: Some(MutationKind::Insert),
            order: 1,
            seq: 1,
            ordinal: 0,
            row_i: 0,
        }),
        Op::Failpoint(FailpointName::AfterCompactionSink),
        Op::CompactThru(1),
    ]);
    assert_eq!(compaction_state(&world, 1), Some(CompactionState::Sent));
    assert_ne!(compaction_state(&world, 1), Some(CompactionState::Acked));
}

fn leased_pipeline(
    dir: &std::path::Path,
    pipeline: &str,
) -> (
    std::sync::Arc<crate::buffer::wal_handle::PipelineWalHandle>,
    CatalogFence,
) {
    use crate::buffer::wal_handle::{catalog_fence_from_guard, memory_catalog_for_test};
    use crate::helpers::offset_store::SledOffsetStore;
    use skippr_lease::{LeaseGuard, PipelineKey, PipelinePaths, SystemClock};
    use std::sync::Arc;
    let key = PipelineKey::new("t", "w", pipeline).unwrap();
    let paths = PipelinePaths::legacy_disk(dir);
    let guard = LeaseGuard::single_node(key.clone(), Arc::new(SystemClock::new()));
    let fence = catalog_fence_from_guard(&guard).unwrap();
    let catalog = memory_catalog_for_test(fence);
    let db = sled::open(dir.join("offsets")).unwrap();
    let offsets: Arc<dyn crate::helpers::offset_store::OffsetStore> =
        Arc::new(SledOffsetStore::new(db.open_tree("o").unwrap()));
    let handle =
        crate::buffer::wal_handle::PipelineWalHandle::new(key, paths, guard, catalog, offsets);
    (handle, fence)
}

#[test]
fn c_sent_hydrate() {
    use crate::buffer::compaction_transaction::{
        CompactionTransaction, CompactionTransactionState, SinkWriteSemantics,
    };
    use crate::buffer::ingest_buffer::Buffers;
    use crate::plugins::source_contract::WritePolicy;
    use skippr_wal_catalog::{
        CatalogKey, CatalogRow, CatalogTxn, CompactionId, CompactionRow, CompactionState, PutCond,
    };
    let dir = tempfile::tempdir().unwrap();
    let (handle, fence) = leased_pipeline(dir.path(), "sent-hydrate");
    let body = serde_json::to_vec(
        &CompactionTransaction::new(
            "sink.a".into(),
            "ns".into(),
            "v1".into(),
            WritePolicy::Append,
            SinkWriteSemantics::AtLeastOnce,
            Vec::new(),
            "out.parquet".into(),
        )
        .with_state(CompactionTransactionState::Sent),
    )
    .unwrap();
    let id = CompactionId::new("sent-1").unwrap();
    let mut txn = CatalogTxn::new();
    txn.put(
        CatalogKey::Compaction(id.clone()),
        CatalogRow::Compaction(CompactionRow {
            id: id.clone(),
            state: CompactionState::Sent,
            body,
            version: 0,
        }),
        PutCond::NotExists,
    )
    .unwrap();
    handle.catalog().commit(&txn, fence).unwrap();
    Buffers::hydrate_from_catalog(&handle, &crate::helpers::configuration::Config::new());
    match handle.catalog().get(&CatalogKey::Compaction(id)).unwrap() {
        Some(CatalogRow::Compaction(row)) => {
            assert_eq!(
                row.state,
                CompactionState::Sent,
                "hydrate must not rewrite Sent in catalog"
            )
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn c_redo_acked() {
    use skippr_wal_catalog::{
        CatalogKey, CatalogRow, CatalogTxn, CompactionId, CompactionRow, CompactionState, PutCond,
    };
    let dir = tempfile::tempdir().unwrap();
    let (handle, fence) = leased_pipeline(dir.path(), "redo-acked");
    let id = CompactionId::new("acked-1").unwrap();
    let mut txn = CatalogTxn::new();
    txn.put(
        CatalogKey::Compaction(id.clone()),
        CatalogRow::Compaction(CompactionRow {
            id: id.clone(),
            state: CompactionState::Acked,
            body: serde_json::to_vec(
                &crate::buffer::compaction_transaction::CompactionTransaction::new(
                    "sink.a".into(),
                    "ns".into(),
                    "v1".into(),
                    crate::plugins::source_contract::WritePolicy::Append,
                    crate::buffer::compaction_transaction::SinkWriteSemantics::AtLeastOnce,
                    Vec::new(),
                    "out.parquet".into(),
                ),
            )
            .unwrap(),
            version: 0,
        }),
        PutCond::NotExists,
    )
    .unwrap();
    handle.catalog().commit(&txn, fence).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(handle.redo_acked_compactions()).unwrap();
    match handle.catalog().get(&CatalogKey::Compaction(id)).unwrap() {
        Some(CatalogRow::Compaction(row)) => assert_eq!(row.state, CompactionState::Acked),
        other => panic!("{other:?}"),
    }
}

#[test]
fn c_persist_matching_sha_skip() {
    use skippr_wal_catalog::{
        CatalogError, CatalogKey, CatalogRow, CatalogTxn, PutCond, WalCatalogRead, WalCatalogStore,
    };
    let store = MemoryWalCatalog::new();
    let f = fence();
    store.install_lease(f.owner, f.epoch);
    let key = CatalogKey::Segment(skippr_lease::SegmentId::new("seg-sha").unwrap());
    let row = occ_segment("seg-sha", 0);
    let mut create = CatalogTxn::new();
    create
        .put(
            key.clone(),
            CatalogRow::Segment(row.clone()),
            PutCond::NotExists,
        )
        .unwrap();
    store.commit(&create, f).unwrap();
    let mut same = CatalogTxn::new();
    same.put(
        key.clone(),
        CatalogRow::Segment(row.clone()),
        PutCond::SameSha,
    )
    .unwrap();
    store.commit(&same, f).unwrap();
    match store.get(&key).unwrap() {
        Some(CatalogRow::Segment(got)) => assert_eq!(got.version, 0),
        other => panic!("{other:?}"),
    }
    let mut different = row;
    different.payload_sha256 = [9; 32];
    let mut rewrite = CatalogTxn::new();
    rewrite
        .put(key, CatalogRow::Segment(different), PutCond::SameSha)
        .unwrap();
    assert_eq!(
        store.commit(&rewrite, f).unwrap_err(),
        CatalogError::ConditionFailed
    );
}

#[test]
fn c_hydrate_without_segf() {
    use crate::buffer::ingest_buffer::Buffers;
    use skippr_wal_catalog::{CatalogKey, CatalogRow, CatalogTxn, PutCond};
    let dir = tempfile::tempdir().unwrap();
    let (handle, fence) = leased_pipeline(dir.path(), "hydrate-junk");
    std::fs::create_dir_all(handle.paths().segs.as_path()).unwrap();
    let path = handle.paths().segs.join("junk.seg");
    std::fs::write(&path, b"not-segf").unwrap();
    let mut row = occ_segment("junk", 0);
    row.location = skippr_wal_catalog::PayloadLocation::Disk {
        path: path.to_string_lossy().into_owned(),
    };
    let key = CatalogKey::Segment(row.segment_id.clone());
    let mut txn = CatalogTxn::new();
    txn.put(key.clone(), CatalogRow::Segment(row), PutCond::NotExists)
        .unwrap();
    handle.catalog().commit(&txn, fence).unwrap();
    Buffers::hydrate_from_catalog(&handle, &crate::helpers::configuration::Config::new());
    assert!(
        handle.catalog().get(&key).unwrap().is_some(),
        "unreadable payload must keep the catalog row"
    );
}
