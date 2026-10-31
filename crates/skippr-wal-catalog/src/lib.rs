//! Skippr Store: local KV for WAL catalog, offsets, stream state, schema, journal.

mod apply;
mod error;
mod fjall_kv;
mod key;
mod kv;
#[cfg(test)]
mod kv_conformance;
mod memory_kv;
mod row;
mod sled_migrate;
mod store;
mod txn;

pub mod conformance;

pub use apply::validate_put;
pub use error::CatalogError;
pub use fjall_kv::FjallSkipprStore;
pub use key::{CatalogKey, CatalogPrefix, CheckpointKey, CompactionId, OffsetHintKey};
pub use kv::{family_rest, KvOp, SkipprStore, SkipprStoreRead, StoreFamily, STORE_FORMAT_VERSION};
pub use memory_kv::{
    catalog_commit, catalog_delete_prefix, catalog_get, catalog_query_prefix, CatalogView,
    MemorySkipprStore, MemoryWalCatalog,
};
pub use row::{
    CatalogFence, CatalogOp, CatalogRow, CatalogSeq, CommittedCheckpoint, CommittedOffset,
    CompactionRow, CompactionState, ControlBody, ControlRow, DeleteCond, FormatPhase, FormatRow,
    LegacySliceRow, PayloadLocation, PutCond, SegmentIndexRow, SegmentRow, SeqRow, SliceBitmap,
    SliceDirEntry, FORMAT_VERSION, ROW_MAX_BYTES,
};
pub use sled_migrate::{lookup_miss_in_sled, migrate_sled_db, MigrateStatus, SLED_TREES};
pub use store::{query_all, reset_catalog, WalCatalogRead, WalCatalogStore};
pub use txn::{CatalogTxn, TXN_MAX_OPS};

#[cfg(test)]
mod tests {
    use skippr_lease::{LeaseEpoch, NodeId};

    use super::*;

    fn fence() -> CatalogFence {
        CatalogFence {
            owner: NodeId::generate(),
            epoch: LeaseEpoch::new(1),
        }
    }

    #[test]
    fn memory_conformance() {
        let store = MemoryWalCatalog::new();
        let f = fence();
        store.set_writer_fence(f.owner, f.epoch);
        conformance::assert_wal_catalog_conformance(&store, f);
        kv_conformance::assert_skippr_store_conformance(&store, f);
    }

    #[test]
    fn memory_released_lease_is_fenced() {
        let store = MemoryWalCatalog::new();
        let f = fence();
        store.set_writer_fence(f.owner, f.epoch);
        store.revoke_writer_fence();
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
            WalCatalogStore::commit(&store, &txn, f).unwrap_err(),
            CatalogError::Fenced
        );
    }

    #[test]
    fn reset_catalog_drops_seq_and_format() {
        let store = MemoryWalCatalog::new();
        let f = fence();
        store.set_writer_fence(f.owner, f.epoch);
        let mut txn = CatalogTxn::new();
        txn.put(
            CatalogKey::Format,
            CatalogRow::Format(FormatRow {
                version: FORMAT_VERSION,
                phase: FormatPhase::Clean,
                row_version: 0,
            }),
            PutCond::NotExists,
        )
        .unwrap();
        txn.put(
            CatalogKey::Seq,
            CatalogRow::Seq(SeqRow {
                max_seq: CatalogSeq::new(3),
            }),
            PutCond::SeqAdvance,
        )
        .unwrap();
        WalCatalogStore::commit(&store, &txn, f).unwrap();
        reset_catalog(&store, f).unwrap();
        assert!(WalCatalogRead::get(&store, &CatalogKey::Format)
            .unwrap()
            .is_none());
        assert!(WalCatalogRead::get(&store, &CatalogKey::Seq)
            .unwrap()
            .is_none());
    }

    #[test]
    fn fjall_conformance() {
        let dir = tempfile::tempdir().unwrap();
        let f = fence();
        let store = FjallSkipprStore::open(dir.path(), f).unwrap();
        conformance::assert_wal_catalog_conformance(&store, f);
        kv_conformance::assert_skippr_store_conformance(&store, f);
        SkipprStore::flush_durable(&store).unwrap();
    }

    #[test]
    fn fjall_second_writer_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let f = fence();
        let _first = FjallSkipprStore::open(dir.path(), f).unwrap();
        let err = match FjallSkipprStore::open(dir.path(), f) {
            Ok(_) => panic!("second writer must fail closed"),
            Err(err) => err,
        };
        assert!(
            matches!(err, CatalogError::Io(ref msg) if msg.contains("already writing")),
            "second writer must fail closed, got {err}"
        );
    }

    #[test]
    fn fjall_open_read_pin_does_not_see_later_writes() {
        let dir = tempfile::tempdir().unwrap();
        let f = fence();
        let store = FjallSkipprStore::open(dir.path(), f).unwrap();
        kv_conformance::assert_skippr_store_conformance(&store, f);
    }

    #[test]
    fn sled_migrate_copies_trees_and_archives() {
        let root = tempfile::tempdir().unwrap();
        let sled_path = root.path().join("db");
        let db = sled::open(&sled_path).unwrap();
        db.open_tree("offsets")
            .unwrap()
            .insert(b"ns-p", b"off")
            .unwrap();
        db.open_tree("offsets")
            .unwrap()
            .insert(b"migration#ws#p", b"journal")
            .unwrap();
        db.open_tree("wal_catalog")
            .unwrap()
            .insert(b"wal#seq", b"seq")
            .unwrap();
        db.open_tree("stream_state")
            .unwrap()
            .insert(b"meta/p/id", b"st")
            .unwrap();
        db.flush().unwrap();
        drop(db);

        let f = fence();
        let store = FjallSkipprStore::open(root.path().join("store"), f).unwrap();
        loop {
            match migrate_sled_db(&store, f, &sled_path, 1).unwrap() {
                MigrateStatus::Complete { archived_to } => {
                    assert!(archived_to.exists());
                    assert!(!sled_path.exists());
                    break;
                }
                MigrateStatus::InProgress { .. } => {}
                MigrateStatus::NoSled => panic!("sled should exist"),
            }
        }
        assert_eq!(
            SkipprStoreRead::get(&store, &StoreFamily::Offsets.key(b"ns-p"))
                .unwrap()
                .as_deref(),
            Some(b"off".as_slice())
        );
        assert_eq!(
            SkipprStoreRead::get(&store, &StoreFamily::Journal.key(b"migration#ws#p"))
                .unwrap()
                .as_deref(),
            Some(b"journal".as_slice())
        );
        assert_eq!(
            SkipprStoreRead::get(&store, &StoreFamily::Offsets.key(b"migration#ws#p")).unwrap(),
            None,
            "leftover sled journal keys must not stay in offsets"
        );
        assert_eq!(
            SkipprStoreRead::get(&store, &StoreFamily::StreamState.key(b"meta/p/id"))
                .unwrap()
                .as_deref(),
            Some(b"st".as_slice())
        );
    }

    #[test]
    fn sled_migrate_miss_fallback_during_copy() {
        let root = tempfile::tempdir().unwrap();
        let sled_path = root.path().join("db");
        let db = sled::open(&sled_path).unwrap();
        let tree = db.open_tree("offsets").unwrap();
        for i in 0..80u32 {
            tree.insert(
                format!("k{i:03}").into_bytes(),
                format!("v{i}").into_bytes(),
            )
            .unwrap();
        }
        tree.insert(b"migration#ws#p", b"journal").unwrap();
        db.flush().unwrap();
        drop(tree);
        drop(db);

        let f = fence();
        let store = FjallSkipprStore::open(root.path().join("store"), f).unwrap();
        match migrate_sled_db(&store, f, &sled_path, 1).unwrap() {
            MigrateStatus::InProgress { tree, .. } => assert_eq!(tree, "offsets"),
            other => panic!("expected in-progress copy, got {other:?}"),
        }
        assert_eq!(
            SkipprStoreRead::get(&store, &StoreFamily::Offsets.key(b"k000"))
                .unwrap()
                .as_deref(),
            Some(b"v0".as_slice())
        );
        assert_eq!(
            SkipprStoreRead::get(&store, &StoreFamily::Offsets.key(b"k079")).unwrap(),
            None,
            "keys past the copy batch stay in sled until later batches"
        );
        assert_eq!(
            lookup_miss_in_sled(&sled_path, StoreFamily::Offsets, b"k079")
                .unwrap()
                .as_deref(),
            Some(b"v79".as_slice())
        );
        assert_eq!(
            lookup_miss_in_sled(&sled_path, StoreFamily::Journal, b"migration#ws#p")
                .unwrap()
                .as_deref(),
            Some(b"journal".as_slice())
        );
        SkipprStore::commit(
            &store,
            &[KvOp::Put {
                key: StoreFamily::Offsets.key(b"live"),
                value: b"ingest".to_vec(),
            }],
            f,
        )
        .unwrap();
        assert_eq!(
            SkipprStoreRead::get(&store, &StoreFamily::Offsets.key(b"live"))
                .unwrap()
                .as_deref(),
            Some(b"ingest".as_slice()),
            "ingest against fjall continues while sled copy is in progress"
        );
    }

    #[test]
    fn sled_migrate_crash_resume_from_cursor() {
        let root = tempfile::tempdir().unwrap();
        let sled_path = root.path().join("db");
        let db = sled::open(&sled_path).unwrap();
        let tree = db.open_tree("offsets").unwrap();
        for i in 0..80u32 {
            tree.insert(
                format!("k{i:03}").into_bytes(),
                format!("v{i}").into_bytes(),
            )
            .unwrap();
        }
        db.flush().unwrap();
        drop(tree);
        drop(db);

        let f = fence();
        let store_path = root.path().join("store");
        {
            let store = FjallSkipprStore::open(&store_path, f).unwrap();
            match migrate_sled_db(&store, f, &sled_path, 1).unwrap() {
                MigrateStatus::InProgress { .. } => {}
                other => panic!("expected in-progress, got {other:?}"),
            }
            drop(store);
        }
        let store = FjallSkipprStore::open(&store_path, f).unwrap();
        loop {
            match migrate_sled_db(&store, f, &sled_path, 1).unwrap() {
                MigrateStatus::Complete { .. } => break,
                MigrateStatus::InProgress { .. } => {}
                MigrateStatus::NoSled => panic!("sled should still exist until complete"),
            }
        }
        assert_eq!(
            SkipprStoreRead::get(&store, &StoreFamily::Offsets.key(b"k079"))
                .unwrap()
                .as_deref(),
            Some(b"v79".as_slice())
        );
    }

    #[test]
    fn leftover_sled_after_archive_is_ignored() {
        let root = tempfile::tempdir().unwrap();
        let archived = root.path().join("db.sled-migrated-3");
        std::fs::create_dir_all(&archived).unwrap();
        let f = fence();
        let store = FjallSkipprStore::open(root.path().join("store"), f).unwrap();
        assert_eq!(
            migrate_sled_db(&store, f, &archived, 3).unwrap(),
            MigrateStatus::NoSled
        );
    }
}
