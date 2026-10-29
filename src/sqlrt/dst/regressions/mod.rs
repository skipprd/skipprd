//! Shrunk cargo-fuzz crashes become named World scripts in [`super::scripts`].

use std::collections::BTreeSet;

const LANDED: &[&str] = &[
    "a_crash_before_catalog_drops_ingest",
    "a_crash_after_catalog_republishes_offsets",
    "a_deadletter_unreadable_json_does_not_advance_offset",
    "c_crash_before_catalog_commit",
    "c_after_catalog_before_offsets",
    "c_after_offsets_published_crash_reopen",
    "c_before_compaction_sink_aborts_at_boundary",
    "c_release_compaction_then_compact",
];

#[test]
fn huge_dst1_op_count_is_not_a_schedule() {
    // cargo-fuzz world_schedule crash-7fd1adbc7c3eaf93b76ed47fe59c65e61de266be
    let bytes: &[u8] = &[
        68, 83, 84, 49, 5, 0, 0, 251, 0, 1, 0, 0, 0, 0, 0, 0, 0, 4, 0, 107, 101, 112, 116, 1, 0, 0,
        0, 0, 0, 0, 0, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 8, 1, 2,
        0, 0, 0, 0, 0, 0, 0, 6, 0, 112, 97, 117, 115, 101, 100, 2, 0, 0, 0, 0, 0, 0, 0, 1, 1, 2, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 9, 1, 3, 0, 0, 0, 0, 0, 0, 0, 5, 0,
        97, 102, 116, 101, 114, 3, 0, 0, 0, 0, 0, 0, 0, 1, 1, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0,
    ];
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        0xfb000005
    );
    assert!(super::schedule_from_bytes(bytes).is_empty());
}

#[test]
fn huge_compact_thru_seq_is_not_a_schedule() {
    let bytes = super::encode_ops(&[super::Op::CompactThru(u64::MAX)]);
    assert!(super::schedule_from_bytes(&bytes).is_empty());
    let world = super::run(&[super::Op::CompactThru(u64::MAX)]);
    let _ = super::current_table(&world);
    assert!(super::compacted_seqs(&world).len() <= super::MAX_SCHEDULE_OPS);
}

#[test]
fn ff_padded_dst1_schedules_do_not_abort() {
    // cargo-fuzz world_schedule crash-3c1f58c0 / crash-ad5f6166 shape:
    // DST1 + small count + 0xFF padding (CompactThru/seq become u64::MAX).
    for count in [1u32, 3] {
        let mut bytes = Vec::from(*b"DST1");
        bytes.extend_from_slice(&count.to_le_bytes());
        bytes.extend(std::iter::repeat_n(0xff, 64));
        let world = super::run(&super::schedule_from_bytes(&bytes));
        let _ = super::current_table(&world);
        assert!(super::compacted_seqs(&world).len() <= super::MAX_SCHEDULE_OPS);
    }
}

#[test]
fn world_schedule_crash_0b179261_is_not_a_schedule() {
    // cargo-fuzz world_schedule crash-0b179261f232b6964a73ce080d6db23dadfd005d
    let bytes: &[u8] = include_bytes!("../../../../fuzz/corpus/world_schedule/crash_0b179261");
    assert!(super::schedule_from_bytes(bytes).is_empty());
    let world = super::run(&super::schedule_from_bytes(bytes));
    let _ = super::current_table(&world);
}

fn wal_event(seq: u64, ordinal: u32, row_i: u64) -> super::WalEvent {
    super::WalEvent {
        row: super::UserRow {
            id: 1,
            name: "a".into(),
            amount: 1,
        },
        mutation: None,
        order: 0,
        seq,
        ordinal,
        row_i,
    }
}

#[test]
fn huge_wal_ordinal_is_not_a_schedule() {
    let bytes = super::encode_ops(&[super::Op::IngestWal(wal_event(0, u32::MAX, 0))]);
    assert!(super::schedule_from_bytes(&bytes).is_empty());
}

#[test]
fn huge_wal_row_i_is_not_a_schedule() {
    let bytes = super::encode_ops(&[super::Op::IngestWal(wal_event(0, 0, u64::MAX))]);
    assert!(super::schedule_from_bytes(&bytes).is_empty());
}

#[test]
fn dst1_bounds_accept_max_schedule_ops_and_reject_one_past() {
    let max = super::MAX_SCHEDULE_OPS as u64;
    let ok_count: Vec<_> = (0..super::MAX_SCHEDULE_OPS)
        .map(|i| {
            super::Op::IngestLake(super::UserRow {
                id: i as i64,
                name: "a".into(),
                amount: 1,
            })
        })
        .collect();
    assert_eq!(
        super::schedule_from_bytes(&super::encode_ops(&ok_count)).len(),
        super::MAX_SCHEDULE_OPS
    );
    let mut over_count = ok_count;
    over_count.push(super::Op::IngestLake(super::UserRow {
        id: max as i64,
        name: "a".into(),
        amount: 1,
    }));
    assert!(super::schedule_from_bytes(&super::encode_ops(&over_count)).is_empty());

    let ok_seq = super::encode_ops(&[super::Op::IngestWal(wal_event(max, 0, 0))]);
    assert_eq!(super::schedule_from_bytes(&ok_seq).len(), 1);
    let bad_seq = super::encode_ops(&[super::Op::IngestWal(wal_event(max + 1, 0, 0))]);
    assert!(super::schedule_from_bytes(&bad_seq).is_empty());

    let max_ord = super::MAX_SCHEDULE_OPS as u32;
    let ok_ord_row = super::encode_ops(&[super::Op::IngestWal(wal_event(0, max_ord, max))]);
    assert_eq!(super::schedule_from_bytes(&ok_ord_row).len(), 1);
    let bad_ord = super::encode_ops(&[super::Op::IngestWal(wal_event(0, max_ord + 1, 0))]);
    assert!(super::schedule_from_bytes(&bad_ord).is_empty());
    let bad_row = super::encode_ops(&[super::Op::IngestWal(wal_event(0, 0, max + 1))]);
    assert!(super::schedule_from_bytes(&bad_row).is_empty());

    let ok_thru = super::encode_ops(&[super::Op::CompactThru(max)]);
    assert_eq!(super::schedule_from_bytes(&ok_thru).len(), 1);
    let bad_thru = super::encode_ops(&[super::Op::CompactThru(max + 1)]);
    assert!(super::schedule_from_bytes(&bad_thru).is_empty());
}

#[test]
fn compacted_seqs_does_not_materialize_u64_max() {
    let mut world = super::World::new();
    world.compacted_thru = u64::MAX;
    assert_eq!(super::compacted_seqs(&world).len(), super::MAX_SCHEDULE_OPS);
}

#[test]
fn regressions_land_as_named_scripts() {
    let names: BTreeSet<_> = super::scripts::all_scripts()
        .into_iter()
        .map(|s| s.name)
        .collect();
    for name in LANDED {
        assert!(names.contains(name), "missing fuzz regression {name}");
    }
}
