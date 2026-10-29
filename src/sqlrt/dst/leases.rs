use crate::helpers::offset_store::apply_offset_field;
use crate::helpers::offsets::OffsetTypes;
use crate::plugins::cdc::{CheckpointAuthority, CheckpointEnvelope, CheckpointKind};

fn dst_migrate_config(
    root: &std::path::Path,
    pipeline: &str,
) -> crate::helpers::configuration::Config {
    use crate::helpers::configuration::{Pipeline, Skippr};
    use crate::helpers::wal_storage::ElStorageMode;
    std::fs::create_dir_all(root).unwrap();
    let mut config = crate::helpers::configuration::Config::new();
    config.skippr = Some(Skippr {
        workspace: Some("dstlease".into()),
        skipprd_el_storage_mode: Some(ElStorageMode::Local),
        ..Skippr::default()
    });
    config.pipelines.insert(
        pipeline.into(),
        Pipeline {
            data_dir: Some(root.display().to_string()),
            ..Pipeline::default()
        },
    );
    config.bind_pipeline(pipeline)
}

#[test]
fn i_activate_ingest_lease_then_persist() {
    use crate::migration::{apply_pending_files, MigrationDirection};
    use crate::pipeline_backend::acquire_ingest_lease;
    let dir = tempfile::tempdir().unwrap();
    let config = dst_migrate_config(dir.path(), "dst-lease-activate");
    let offsets = crate::helpers::offsets::Offsets::init(&config).unwrap();
    let store = offsets.bytes_store().expect("journal store");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let lease = acquire_ingest_lease(&config, &offsets).await.unwrap();
        lease.activate().map_err(|err| err.to_string()).unwrap();
        let applied = apply_pending_files(&config, store.as_ref(), MigrationDirection::Up)
            .await
            .unwrap();
        assert!(applied.is_empty(), "{applied:?}");
        store.flush().unwrap();
    });
}

#[test]
fn i_exclusive_sled_open_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("offsets");
    let _first = sled::open(&path).unwrap();
    let err = sled::open(&path).expect_err("second process must not open the same sled");
    let msg = err.to_string();
    assert!(
        msg.contains("could not acquire lock")
            || msg.contains("already")
            || msg.contains("lock")
            || msg.contains("Io"),
        "{msg}"
    );
}

#[test]
fn i_lease_epoch_stale_writer_rejected() {
    use skippr_lease::{LeaseEpoch, NodeId};
    use skippr_wal_catalog::{
        CatalogFence, CatalogKey, CatalogRow, CatalogTxn, FormatPhase, FormatRow, MemoryWalCatalog,
        PutCond, WalCatalogStore, FORMAT_VERSION,
    };
    let store = MemoryWalCatalog::new();
    let owner = NodeId::generate();
    store.install_lease(owner, LeaseEpoch::new(2));
    let stale = CatalogFence {
        owner,
        epoch: LeaseEpoch::new(1),
    };
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
    assert_eq!(
        store.commit(&txn, stale).unwrap_err(),
        skippr_wal_catalog::CatalogError::Fenced
    );
}

#[test]
fn i_checkpoint_offset_hint_roundtrip() {
    let envelope = CheckpointEnvelope {
        authority: CheckpointAuthority::AdvisoryHint,
        kind: CheckpointKind::AdvisoryProgress,
        payload_version: 1,
        payload_bytes: b"hint".to_vec(),
    };
    let bytes = bincode::serialize(&envelope).unwrap();
    let back: CheckpointEnvelope = bincode::deserialize(&bytes).unwrap();
    assert_eq!(back.kind, CheckpointKind::AdvisoryProgress);
    assert_eq!(back.payload_bytes, b"hint");

    let stored = apply_offset_field(None, OffsetTypes::Position, 42, false).unwrap();
    let again = apply_offset_field(Some(stored.clone()), OffsetTypes::Closed, 1, false).unwrap();
    assert_ne!(stored, again);
    let same = apply_offset_field(Some(stored.clone()), OffsetTypes::Position, 42, false).unwrap();
    assert_eq!(stored, same);
}

#[test]
fn i_offset_position_filesize_closed() {
    let mut bytes = apply_offset_field(None, OffsetTypes::Filesize, 100, false).unwrap();
    bytes = apply_offset_field(Some(bytes), OffsetTypes::Position, 7, false).unwrap();
    bytes = apply_offset_field(Some(bytes), OffsetTypes::Closed, 1, false).unwrap();
    let again = apply_offset_field(Some(bytes.clone()), OffsetTypes::Position, 20, true).unwrap();
    assert_ne!(bytes, again);
    let maxed = apply_offset_field(Some(again.clone()), OffsetTypes::Position, 1, true).unwrap();
    assert_eq!(again, maxed);
}

#[test]
fn i_offset_corrupt_schema() {
    let err = apply_offset_field(Some(vec![1, 2, 3]), OffsetTypes::Closed, 1, false).unwrap_err();
    assert_eq!(err, "offset bytes do not fit schema");
}
