use skippr_lease::{LeaseEpoch, NodeId, SegmentId};

use crate::error::CatalogError;
use crate::key::{CatalogKey, CatalogPrefix, CompactionId};
use crate::row::{
    CatalogFence, CatalogRow, CatalogSeq, CompactionRow, CompactionState, FormatPhase, FormatRow,
    PayloadLocation, PutCond, SegmentRow, SeqRow, SliceBitmap, FORMAT_VERSION,
};
use crate::store::{query_all, WalCatalogStore};
use crate::txn::CatalogTxn;

pub fn assert_wal_catalog_conformance(store: &dyn WalCatalogStore, fence: CatalogFence) {
    not_exists_same_sha_version(store, fence);
    seq_advance(store, fence);
    stale_fence_rejected(store, fence);
    pagination_and_delete_prefix(store, fence);
}

fn not_exists_same_sha_version(store: &dyn WalCatalogStore, fence: CatalogFence) {
    let key = CatalogKey::Segment(SegmentId::new("seg-conf").unwrap());
    let row = segment(1, 0);
    let mut txn = CatalogTxn::new();
    txn.put(
        key.clone(),
        CatalogRow::Segment(row.clone()),
        PutCond::NotExists,
    )
    .unwrap();
    txn.put(
        CatalogKey::Seq,
        CatalogRow::Seq(SeqRow {
            max_seq: CatalogSeq::new(1),
        }),
        PutCond::SeqAdvance,
    )
    .unwrap();
    store.commit(&txn, fence).unwrap();

    let mut again = CatalogTxn::new();
    again
        .put(
            key.clone(),
            CatalogRow::Segment(row.clone()),
            PutCond::NotExists,
        )
        .unwrap();
    assert_eq!(
        store.commit(&again, fence).unwrap_err(),
        CatalogError::ConditionFailed
    );

    let mut same = CatalogTxn::new();
    same.put(
        key.clone(),
        CatalogRow::Segment(row.clone()),
        PutCond::SameSha,
    )
    .unwrap();
    store.commit(&same, fence).unwrap();

    let mut bumped = row.clone();
    bumped.version = 1;
    let mut wrong_ver = CatalogTxn::new();
    wrong_ver
        .put(
            key.clone(),
            CatalogRow::Segment(bumped.clone()),
            PutCond::Version(7),
        )
        .unwrap();
    assert_eq!(
        store.commit(&wrong_ver, fence).unwrap_err(),
        CatalogError::ConditionFailed
    );

    let mut ok_ver = CatalogTxn::new();
    ok_ver
        .put(
            key.clone(),
            CatalogRow::Segment(bumped),
            PutCond::Version(0),
        )
        .unwrap();
    store.commit(&ok_ver, fence).unwrap();
    match store.get(&key).unwrap() {
        Some(CatalogRow::Segment(got)) => assert_eq!(got.version, 1),
        other => panic!("expected segment, got {other:?}"),
    }
}

fn seq_advance(store: &dyn WalCatalogStore, fence: CatalogFence) {
    let mut lower = CatalogTxn::new();
    lower
        .put(
            CatalogKey::Seq,
            CatalogRow::Seq(SeqRow {
                max_seq: CatalogSeq::new(1),
            }),
            PutCond::SeqAdvance,
        )
        .unwrap();
    assert_eq!(
        store.commit(&lower, fence).unwrap_err(),
        CatalogError::ConditionFailed
    );
    let mut higher = CatalogTxn::new();
    higher
        .put(
            CatalogKey::Seq,
            CatalogRow::Seq(SeqRow {
                max_seq: CatalogSeq::new(2),
            }),
            PutCond::SeqAdvance,
        )
        .unwrap();
    store.commit(&higher, fence).unwrap();
}

fn stale_fence_rejected(store: &dyn WalCatalogStore, fence: CatalogFence) {
    let foreign = CatalogFence {
        owner: NodeId::generate(),
        epoch: fence.epoch,
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
        store.commit(&txn, foreign).unwrap_err(),
        CatalogError::Fenced
    );
    let stale = CatalogFence {
        owner: fence.owner,
        epoch: LeaseEpoch::new(fence.epoch.get().saturating_add(1)),
    };
    assert_eq!(store.commit(&txn, stale).unwrap_err(), CatalogError::Fenced);
}

fn pagination_and_delete_prefix(store: &dyn WalCatalogStore, fence: CatalogFence) {
    for i in 0..12u32 {
        let id = CompactionId::new(format!("cmp-{i:02}")).unwrap();
        let mut txn = CatalogTxn::new();
        txn.put(
            CatalogKey::Compaction(id.clone()),
            CatalogRow::Compaction(CompactionRow {
                id,
                state: CompactionState::Pending,
                body: vec![i as u8],
                version: 0,
            }),
            PutCond::NotExists,
        )
        .unwrap();
        store.commit(&txn, fence).unwrap();
    }
    let page = store
        .query_prefix(CatalogPrefix::Compaction, None, 5)
        .unwrap();
    assert_eq!(page.len(), 5);
    let next = store
        .query_prefix(CatalogPrefix::Compaction, Some(&page[4].0), 5)
        .unwrap();
    assert_eq!(next.len(), 5);
    let all = query_all(store, CatalogPrefix::Compaction).unwrap();
    assert!(all.len() >= 12);
    store
        .delete_prefix(CatalogPrefix::Compaction, fence)
        .unwrap();
    assert!(query_all(store, CatalogPrefix::Compaction)
        .unwrap()
        .is_empty());
}

fn segment(seq: u64, version: u64) -> SegmentRow {
    SegmentRow {
        segment_id: SegmentId::new("seg-conf").unwrap(),
        seq: CatalogSeq::new(seq),
        location: PayloadLocation::Disk {
            path: "/tmp/seg-conf.seg".into(),
        },
        payload_len: 1,
        payload_sha256: [1; 32],
        slice_count: 1,
        done: SliceBitmap::new(1),
        offsets: vec![],
        checkpoints: vec![],
        epoch: LeaseEpoch::new(1),
        version,
    }
}
