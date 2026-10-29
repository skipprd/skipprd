use crate::buffer::compaction_index::{
    CompactionGroupKey, CompactionIndex, CompactionKind, CompactionSliceId, IndexedCompactionSlice,
};
use crate::buffer::compaction_transaction::{
    CompactionTransaction, CompactionTransactionState, SegmentSourceDescriptor, SinkWriteSemantics,
    WalPartRef,
};
use crate::buffer::segment_file::{
    PartitionKey, SegmentPartMetaSummary, SegmentPartitionIndexEntry,
};
use crate::plugins::source_contract::WritePolicy;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

fn tiny_slice_ns(segment: &str, start: u64, bytes: u64, namespace: &str) -> IndexedCompactionSlice {
    let key = PartitionKey {
        sink_ref: "sink.a".into(),
        namespace: namespace.into(),
        partition: "p=1".into(),
        time: Some(100),
        schema_fingerprint: "v1".into(),
    };
    let index_entry = SegmentPartitionIndexEntry {
        key: key.clone(),
        bytes,
        updated_at_secs: 1_000,
        slice_ordinal: start as u32,
        part_meta_start: 0,
        part_meta_len: 0,
        part_meta_summary: SegmentPartMetaSummary::Append,
        codec: crate::buffer::segment_file::PART_CODEC_ARROW,
        start,
        len: bytes,
    };
    let wal_ref = WalPartRef {
        segment_id: segment.into(),
        source: SegmentSourceDescriptor::Disk {
            path: PathBuf::from(format!("/tmp/{segment}.seg")),
        },
        start,
        len: bytes,
        key,
        cdc_meta_hash: index_entry.part_meta_summary.canonical_hash(),
        ordinal: Some(index_entry.slice_ordinal),
    };
    IndexedCompactionSlice {
        id: CompactionSliceId::from_wal_ref(&wal_ref),
        group_key: CompactionGroupKey {
            sink_ref: "sink.a".into(),
            namespace: namespace.into(),
            partition: "p=1".into(),
            time: Some(100),
            schema_fingerprint: "v1".into(),
            kind: CompactionKind::Append,
        },
        index_entry,
        wal_ref,
    }
}

fn tiny_slice(segment: &str, start: u64, bytes: u64) -> IndexedCompactionSlice {
    tiny_slice_ns(segment, start, bytes, "ns")
}

fn plan_group(index: &mut CompactionIndex, now_secs: u64, time_threshold_secs: u64) -> usize {
    index
        .plan_ready(
            3,
            false,
            now_secs,
            100,
            time_threshold_secs,
            250,
            8,
            1,
            HashMap::new(),
            HashSet::new(),
        )
        .groups
        .len()
}

#[test]
fn a_tiny_slices_wait_group_bytes() {
    let mut index = CompactionIndex::default();
    index.register_segment(
        "seg-a",
        vec![tiny_slice("seg-a", 0, 40)],
        100,
        10_000,
        1_000,
    );
    index.register_segment(
        "seg-b",
        vec![tiny_slice("seg-b", 1, 40)],
        100,
        10_000,
        1_000,
    );
    assert_eq!(plan_group(&mut index, 1_000, 10_000), 0);
    index.register_segment(
        "seg-c",
        vec![tiny_slice("seg-c", 2, 40)],
        100,
        10_000,
        1_000,
    );
    assert_eq!(plan_group(&mut index, 1_000, 10_000), 1);
}

#[test]
fn a_group_bytes_compact_without_time() {
    let mut index = CompactionIndex::default();
    for (segment, start, bytes) in [("seg-a", 0, 40u64), ("seg-b", 1, 40), ("seg-c", 2, 40)] {
        index.register_segment(
            segment,
            vec![tiny_slice(segment, start, bytes)],
            100,
            10_000,
            1_000,
        );
    }
    assert_eq!(plan_group(&mut index, 1_000, 10_000), 1);
}

#[test]
fn a_compaction_time_threshold_readiness() {
    let mut index = CompactionIndex::default();
    index.register_segment(
        "seg-a",
        vec![tiny_slice("seg-a", 0, 40)],
        100,
        10_000,
        1_000,
    );
    assert_eq!(plan_group(&mut index, 1_000, 10_000), 0);
    assert_eq!(plan_group(&mut index, 20_000, 1), 1);
}

#[test]
fn a_slice_oversize_planned_alone() {
    let mut index = CompactionIndex::default();
    index.register_segment(
        "seg-big",
        vec![tiny_slice("seg-big", 0, 500)],
        100,
        10_000,
        1_000,
    );
    assert_eq!(plan_group(&mut index, 1_000, 10_000), 1);
}

#[test]
fn a_group_bytes_do_not_cross_namespace() {
    let mut index = CompactionIndex::default();
    index.register_segment(
        "seg-a",
        vec![tiny_slice_ns("seg-a", 0, 80, "ns-a")],
        100,
        10_000,
        1_000,
    );
    index.register_segment(
        "seg-b",
        vec![tiny_slice_ns("seg-b", 1, 80, "ns-b")],
        100,
        10_000,
        1_000,
    );
    let groups = index
        .plan_ready(
            8,
            false,
            1_000,
            100,
            10_000,
            250,
            8,
            8,
            HashMap::new(),
            HashSet::new(),
        )
        .groups
        .len();
    assert_eq!(groups, 0, "80+80 across namespaces must not share a group");
}

#[test]
fn a_per_sink_namespace_hard_cap_holds() {
    let mut index = CompactionIndex::default();
    for (seg, start) in [("seg-a", 0u64), ("seg-b", 1), ("seg-c", 2)] {
        index.register_segment(seg, vec![tiny_slice(seg, start, 120)], 100, 10_000, 1_000);
    }
    let groups = index
        .plan_ready(
            8,
            false,
            1_000,
            100,
            10_000,
            250,
            8,
            1,
            HashMap::new(),
            HashSet::new(),
        )
        .groups
        .len();
    assert_eq!(groups, 1, "per-sink cap 1 allows one group");
}

#[test]
fn a_slice_bitmap_partial_done_blocks_reclaim() {
    use skippr_lease::{LeaseEpoch, SegmentId};
    use skippr_wal_catalog::{
        CatalogKey, CatalogRow, CatalogSeq, CatalogTxn, PayloadLocation, PutCond, SegmentRow,
        SliceBitmap,
    };
    let mut bits = SliceBitmap::new(2);
    bits.or_bits(&[0]);
    assert!(bits.is_done(0));
    assert!(!bits.is_done(1));
    assert!(
        !bits.all_done(),
        "reclaim_fully_done_before skips unless all_done"
    );

    use crate::buffer::wal_handle::{catalog_fence_from_guard, memory_catalog_for_test};
    use crate::helpers::offset_store::SledOffsetStore;
    use skippr_lease::{LeaseGuard, PipelineKey, PipelinePaths, SystemClock};
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let key = PipelineKey::new("t", "w", "partial-reclaim").unwrap();
    let paths = PipelinePaths::legacy_disk(dir.path());
    let guard = LeaseGuard::single_node(key.clone(), Arc::new(SystemClock::new()));
    let fence = catalog_fence_from_guard(&guard).unwrap();
    let catalog = memory_catalog_for_test(fence);
    let db = sled::open(dir.path().join("offsets")).unwrap();
    let offsets: Arc<dyn crate::helpers::offset_store::OffsetStore> =
        Arc::new(SledOffsetStore::new(db.open_tree("o").unwrap()));
    let handle =
        crate::buffer::wal_handle::PipelineWalHandle::new(key, paths, guard, catalog, offsets);
    let mut bits = SliceBitmap::new(2);
    bits.or_bits(&[0]);
    let id = SegmentId::new("live-partial").unwrap();
    let mut txn = CatalogTxn::new();
    txn.put(
        CatalogKey::Segment(id.clone()),
        CatalogRow::Segment(SegmentRow {
            segment_id: id.clone(),
            seq: CatalogSeq::new(1),
            location: PayloadLocation::Disk {
                path: "/tmp/live-partial.seg".into(),
            },
            payload_len: 1,
            payload_sha256: [2; 32],
            slice_count: 2,
            done: bits,
            offsets: vec![],
            checkpoints: vec![],
            epoch: LeaseEpoch::new(1),
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
    rt.block_on(handle.reclaim_fully_done_before(u64::MAX))
        .unwrap();
    assert!(
        handle
            .catalog()
            .get(&CatalogKey::Segment(id))
            .unwrap()
            .is_some(),
        "partial SliceBitmap must block reclaim_fully_done_before"
    );
}

#[test]
fn a_compaction_sent_fence() {
    let slice = tiny_slice("seg-a", 0, 500);
    let mut txn = CompactionTransaction::new(
        "sink.a".into(),
        "ns".into(),
        "v1".into(),
        WritePolicy::Append,
        SinkWriteSemantics::AtLeastOnce,
        vec![slice.wal_ref.clone()],
        "out.parquet".into(),
    );
    txn.state = CompactionTransactionState::Sent;
    let mut index = CompactionIndex::default();
    index.install_manifests(vec![txn]);
    index.register_segment("seg-a", vec![slice], 100, 10_000, 1_000);
    assert_eq!(
        plan_group(&mut index, 1_000, 10_000),
        0,
        "Sent compaction must fence overlapping slices"
    );
}
