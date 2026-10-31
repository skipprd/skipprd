//! WAL catalog: the skipprd store is the only authority for live segments,
//! delivered slices, compaction state, reclaim, and the commit sequence.

mod apply;
mod error;
mod key;
mod memory;
mod row;
mod sled_store;
mod store;
mod txn;

pub mod conformance;

pub use apply::validate_put;
pub use error::CatalogError;
pub use key::{CatalogKey, CatalogPrefix, CheckpointKey, CompactionId, OffsetHintKey};
pub use memory::MemoryWalCatalog;
pub use row::{
    CatalogFence, CatalogOp, CatalogRow, CatalogSeq, CommittedCheckpoint, CommittedOffset,
    CompactionRow, CompactionState, ControlBody, ControlRow, DeleteCond, FormatPhase, FormatRow,
    LegacySliceRow, PayloadLocation, PutCond, SegmentIndexRow, SegmentRow, SeqRow, SliceBitmap,
    SliceDirEntry, FORMAT_VERSION, ROW_MAX_BYTES,
};
pub use sled_store::SledWalCatalog;
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
        assert_eq!(store.commit(&txn, f).unwrap_err(), CatalogError::Fenced);
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
        store.commit(&txn, f).unwrap();
        reset_catalog(&store, f).unwrap();
        assert!(store.get(&CatalogKey::Format).unwrap().is_none());
        assert!(store.get(&CatalogKey::Seq).unwrap().is_none());
    }

    #[test]
    fn sled_conformance() {
        let dir = tempfile::tempdir().unwrap();
        let db = sled::open(dir.path()).unwrap();
        let tree = db.open_tree("wal_catalog").unwrap();
        let f = fence();
        let store = SledWalCatalog::open(tree, f);
        conformance::assert_wal_catalog_conformance(&store, f);
        store.flush_durable().unwrap();
    }

    #[test]
    fn sled_open_read_rejects_commit() {
        let dir = tempfile::tempdir().unwrap();
        let db = sled::open(dir.path()).unwrap();
        let tree = db.open_tree("wal_catalog").unwrap();
        let store = SledWalCatalog::open_read(tree);
        let mut txn = CatalogTxn::new();
        txn.put(
            CatalogKey::Seq,
            CatalogRow::Seq(SeqRow {
                max_seq: CatalogSeq::new(1),
            }),
            PutCond::NotExists,
        )
        .unwrap();
        let err = store.commit(&txn, fence()).unwrap_err();
        assert!(
            matches!(err, CatalogError::Fenced),
            "query-opened sled catalog must not commit, got {err}"
        );
    }
}
