use super::*;
use crate::cluster::failpoint::FailpointName;
use crate::plugins::cdc::MutationKind;
use crate::sqlrt::read_boundary::LakeSnapshot;
use skippr_wal_catalog::{CatalogPrefix, CatalogRow, CompactionState, WalCatalogRead};
use std::collections::{BTreeMap, BTreeSet};

fn row(id: i64, name: &str, amount: i64) -> UserRow {
    UserRow {
        id,
        name: name.into(),
        amount,
    }
}

fn wal(id: i64, name: &str, amount: i64, mutation: MutationKind, order: u8, seq: u64) -> WalEvent {
    WalEvent {
        row: row(id, name, amount),
        mutation: Some(mutation),
        order,
        seq,
        ordinal: 0,
        row_i: 0,
    }
}

pub struct Script {
    pub name: &'static str,
    pub ops: Vec<Op>,
    pub expect: BTreeMap<i64, (String, i64)>,
    pub expect_drained: Option<bool>,
    pub expect_offsets: Option<u64>,
    pub expect_count: Option<usize>,
    pub expect_sum: Option<i64>,
}

fn expect(pairs: &[(i64, &str, i64)]) -> BTreeMap<i64, (String, i64)> {
    pairs
        .iter()
        .map(|(id, name, amount)| (*id, ((*name).to_string(), *amount)))
        .collect()
}

const OVERLAP_N: i64 = 8;

pub fn all_scripts() -> Vec<Script> {
    use MutationKind::*;
    vec![
        Script {
            name: "a_empty_lake_insert_count",
            ops: vec![Op::IngestWal(wal(1, "a", 10, Insert, 1, 1))],
            expect: expect(&[(1, "a", 10)]),
            expect_drained: Some(false),
            expect_offsets: None,
            expect_count: Some(1),
            expect_sum: Some(10),
        },
        Script {
            name: "a_two_namespaces_as_two_keys",
            ops: vec![
                Op::IngestWal(wal(1, "detail-type-A", 1, Insert, 1, 1)),
                Op::IngestWal(wal(2, "detail-type-B", 2, Insert, 1, 2)),
            ],
            expect: expect(&[(1, "detail-type-A", 1), (2, "detail-type-B", 2)]),
            expect_drained: None,
            expect_offsets: None,
            expect_count: None,
            expect_sum: None,
        },
        Script {
            name: "a_compact_first_day_keeps_second",
            ops: vec![
                Op::IngestWal(wal(1, "d1", 1, Insert, 1, 1)),
                Op::IngestWal(wal(2, "d2", 2, Insert, 1, 2)),
                Op::CompactThru(1),
            ],
            expect: expect(&[(1, "d1", 1), (2, "d2", 2)]),
            expect_drained: Some(false),
            expect_offsets: None,
            expect_count: None,
            expect_sum: None,
        },
        Script {
            name: "a_crash_before_catalog_drops_ingest",
            ops: vec![
                Op::Failpoint(FailpointName::BeforeCatalogCommit),
                Op::IngestWal(wal(1, "lost", 1, Insert, 1, 1)),
                Op::Reopen,
            ],
            expect: BTreeMap::new(),
            expect_drained: Some(true),
            expect_offsets: Some(0),
            expect_count: None,
            expect_sum: None,
        },
        Script {
            name: "a_crash_after_catalog_republishes_offsets",
            ops: vec![
                Op::IngestWal(wal(1, "kept", 7, Insert, 1, 1)),
                Op::Failpoint(FailpointName::AfterCatalogCommit),
                Op::Reopen,
            ],
            expect: expect(&[(1, "kept", 7)]),
            expect_drained: None,
            expect_offsets: Some(1),
            expect_count: None,
            expect_sum: None,
        },
        Script {
            name: "b_uncompacted_snapshot_replaces_lake",
            ops: vec![
                Op::IngestLake(row(1, "alpha", 120)),
                Op::IngestWal(wal(1, "alpha", 100, Snapshot, 1, 1)),
            ],
            expect: expect(&[(1, "alpha", 100)]),
            expect_drained: None,
            expect_offsets: None,
            expect_count: None,
            expect_sum: None,
        },
        Script {
            name: "b_snapshot_fills_empty_lake",
            ops: vec![
                Op::IngestWal(wal(1, "alpha", 100, Snapshot, 1, 1)),
                Op::IngestWal(wal(2, "beta", 50, Snapshot, 1, 2)),
            ],
            expect: expect(&[(1, "alpha", 100), (2, "beta", 50)]),
            expect_drained: None,
            expect_offsets: None,
            expect_count: None,
            expect_sum: None,
        },
        Script {
            name: "b_insert_unions_with_lake",
            ops: vec![
                Op::IngestLake(row(1, "alpha", 10)),
                Op::IngestWal(wal(2, "beta", 20, Insert, 1, 1)),
            ],
            expect: expect(&[(1, "alpha", 10), (2, "beta", 20)]),
            expect_drained: None,
            expect_offsets: None,
            expect_count: None,
            expect_sum: None,
        },
        Script {
            name: "b_update_replaces_lake_row",
            ops: vec![
                Op::IngestLake(row(1, "alpha", 100)),
                Op::IngestWal(wal(1, "alpha-prime", 120, Update, 2, 1)),
            ],
            expect: expect(&[(1, "alpha-prime", 120)]),
            expect_drained: None,
            expect_offsets: None,
            expect_count: None,
            expect_sum: None,
        },
        Script {
            name: "b_delete_removes_key",
            ops: vec![
                Op::IngestLake(row(1, "alpha", 10)),
                Op::IngestLake(row(2, "beta", 20)),
                Op::IngestWal(wal(2, "beta", 20, Delete, 1, 1)),
            ],
            expect: expect(&[(1, "alpha", 10)]),
            expect_drained: None,
            expect_offsets: None,
            expect_count: None,
            expect_sum: None,
        },
        Script {
            name: "b_many_updates_keep_max_order_token",
            ops: vec![
                Op::IngestWal(wal(1, "a", 1, Update, 1, 1)),
                Op::IngestWal(wal(1, "b", 2, Update, 3, 2)),
                Op::IngestWal(wal(1, "c", 3, Update, 2, 3)),
            ],
            expect: expect(&[(1, "b", 2)]),
            expect_drained: None,
            expect_offsets: None,
            expect_count: None,
            expect_sum: None,
        },
        Script {
            name: "b_update_then_delete_absent",
            ops: vec![
                Op::IngestWal(wal(1, "a", 1, Update, 1, 1)),
                Op::IngestWal(wal(1, "a", 1, Delete, 2, 2)),
            ],
            expect: BTreeMap::new(),
            expect_drained: None,
            expect_offsets: None,
            expect_count: None,
            expect_sum: None,
        },
        Script {
            name: "b_delete_then_reinsert",
            ops: vec![
                Op::IngestWal(wal(1, "old", 1, Delete, 1, 1)),
                Op::IngestWal(wal(1, "new", 9, Insert, 2, 2)),
            ],
            expect: expect(&[(1, "new", 9)]),
            expect_drained: None,
            expect_offsets: None,
            expect_count: None,
            expect_sum: None,
        },
        Script {
            name: "b_equal_tokens_tie_break_seq",
            ops: vec![
                Op::IngestWal(WalEvent {
                    row: row(1, "a", 1),
                    mutation: Some(Update),
                    order: 5,
                    seq: 1,
                    ordinal: 0,
                    row_i: 0,
                }),
                Op::IngestWal(WalEvent {
                    row: row(1, "b", 2),
                    mutation: Some(Update),
                    order: 5,
                    seq: 1,
                    ordinal: 1,
                    row_i: 0,
                }),
                Op::IngestWal(WalEvent {
                    row: row(1, "c", 3),
                    mutation: Some(Update),
                    order: 5,
                    seq: 1,
                    ordinal: 1,
                    row_i: 1,
                }),
            ],
            expect: expect(&[(1, "c", 3)]),
            expect_drained: None,
            expect_offsets: None,
            expect_count: None,
            expect_sum: None,
        },
        Script {
            name: "b_compacted_snapshot_leftover_does_not_replace_lake",
            ops: vec![
                Op::IngestLake(row(1, "alpha", 120)),
                Op::CompactThru(1),
                Op::IngestWal(wal(1, "alpha", 100, Snapshot, 1, 1)),
            ],
            expect: expect(&[(1, "alpha", 120)]),
            expect_drained: Some(true),
            expect_offsets: None,
            expect_count: None,
            expect_sum: None,
        },
        Script {
            name: "b_compact_prefix_then_live_update",
            ops: vec![
                Op::IngestLake(row(1, "alpha", 100)),
                Op::IngestWal(wal(1, "alpha", 110, Update, 1, 1)),
                Op::CompactThru(1),
                Op::IngestWal(wal(1, "alpha", 120, Update, 2, 2)),
            ],
            expect: expect(&[(1, "alpha", 120)]),
            expect_drained: Some(false),
            expect_offsets: None,
            expect_count: None,
            expect_sum: None,
        },
        Script {
            name: "b_append_does_not_suppress_lake",
            ops: vec![
                Op::IngestLake(row(1, "alpha", 10)),
                Op::IngestWal(WalEvent {
                    row: row(2, "extra", 1),
                    mutation: None,
                    order: 0,
                    seq: 1,
                    ordinal: 0,
                    row_i: 0,
                }),
            ],
            expect: expect(&[(1, "alpha", 10), (2, "extra", 1)]),
            expect_drained: None,
            expect_offsets: None,
            expect_count: None,
            expect_sum: None,
        },
        Script {
            name: "b_pk_change_delete_old_insert_new",
            ops: vec![
                Op::IngestLake(row(1, "inv", 40)),
                Op::IngestWal(wal(1, "inv", 40, Delete, 1, 1)),
                Op::IngestWal(wal(2, "inv", 40, Insert, 2, 2)),
            ],
            expect: expect(&[(2, "inv", 40)]),
            expect_drained: None,
            expect_offsets: None,
            expect_count: None,
            expect_sum: None,
        },
        Script {
            name: "b_stale_wal_token_loses_to_newer",
            ops: vec![
                Op::IngestWal(wal(1, "new", 9, Insert, 5, 1)),
                Op::IngestWal(wal(1, "old", 1, Update, 1, 2)),
            ],
            expect: expect(&[(1, "new", 9)]),
            expect_drained: None,
            expect_offsets: None,
            expect_count: None,
            expect_sum: None,
        },
        Script {
            name: "b_point_id_1_after_mix",
            ops: vec![
                Op::IngestLake(row(1, "keep", 1)),
                Op::IngestLake(row(2, "gone", 2)),
                Op::IngestWal(wal(2, "gone", 2, Delete, 1, 1)),
                Op::IngestWal(wal(3, "new", 3, Insert, 1, 2)),
            ],
            expect: expect(&[(1, "keep", 1), (3, "new", 3)]),
            expect_drained: None,
            expect_offsets: None,
            expect_count: None,
            expect_sum: None,
        },
        Script {
            name: "h_query_after_compact_is_drained",
            ops: vec![
                Op::IngestWal(wal(1, "a", 1, Insert, 1, 1)),
                Op::CompactThru(1),
            ],
            expect: expect(&[(1, "a", 1)]),
            expect_drained: Some(true),
            expect_offsets: None,
            expect_count: None,
            expect_sum: None,
        },
        Script {
            name: "a_duplicate_payload_sha_does_not_rewrite",
            ops: vec![
                Op::IngestWal(wal(1, "same", 1, Insert, 1, 1)),
                Op::IngestWal(wal(1, "same", 1, Insert, 1, 2)),
            ],
            expect: expect(&[(1, "same", 1)]),
            expect_drained: None,
            expect_offsets: None,
            expect_count: None,
            expect_sum: None,
        },
        Script {
            name: "c_after_offsets_published_idempotent_reopen",
            ops: vec![
                Op::IngestWal(wal(1, "kept", 1, Insert, 1, 1)),
                Op::Reopen,
                Op::Reopen,
            ],
            expect: expect(&[(1, "kept", 1)]),
            expect_offsets: Some(1),
            expect_drained: None,
            expect_count: None,
            expect_sum: None,
        },
        Script {
            name: "c_compaction_acked_is_terminal",
            ops: vec![
                Op::IngestWal(wal(1, "a", 1, Insert, 1, 1)),
                Op::CompactThru(1),
                Op::Reopen,
            ],
            expect: expect(&[(1, "a", 1)]),
            expect_drained: Some(true),
            expect_offsets: None,
            expect_count: None,
            expect_sum: None,
        },
        Script {
            name: "d_copy_identity_append_does_not_cdc_merge",
            ops: vec![
                Op::IngestLake(row(1, "lake", 1)),
                Op::IngestWal(WalEvent {
                    row: row(2, "copy", 1),
                    mutation: None,
                    order: 0,
                    seq: 1,
                    ordinal: 0,
                    row_i: 0,
                }),
            ],
            expect: expect(&[(1, "lake", 1), (2, "copy", 1)]),
            expect_drained: None,
            expect_offsets: None,
            expect_count: None,
            expect_sum: None,
        },
        Script {
            name: "d_namespace_burst_then_identity_put",
            ops: (1..=16)
                .map(|i| {
                    Op::IngestWal(WalEvent {
                        row: row(i, "detail-type", i),
                        mutation: None,
                        order: 0,
                        seq: i as u64,
                        ordinal: 0,
                        row_i: 0,
                    })
                })
                .collect(),
            expect: (1..=16)
                .map(|i| (i, ("detail-type".to_string(), i)))
                .collect(),
            expect_drained: None,
            expect_offsets: None,
            expect_count: None,
            expect_sum: None,
        },
        Script {
            name: "e_add_leaf_old_rows_still_query",
            ops: vec![
                Op::IngestLake(row(1, "before", 1)),
                Op::EvolveAddColumn("leaf".into()),
                Op::IngestWal(wal(2, "after", 2, Insert, 1, 1)),
            ],
            expect: expect(&[(1, "before", 1), (2, "after", 2)]),
            expect_drained: None,
            expect_offsets: None,
            expect_count: None,
            expect_sum: None,
        },
        Script {
            name: "e_null_then_value",
            ops: vec![
                Op::IngestWal(wal(1, "", 0, Insert, 1, 1)),
                Op::IngestWal(wal(1, "set", 9, Update, 2, 2)),
            ],
            expect: expect(&[(1, "set", 9)]),
            expect_drained: None,
            expect_offsets: None,
            expect_count: None,
            expect_sum: None,
        },
        Script {
            name: "h_count_sum_point_on_pin",
            ops: vec![
                Op::IngestLake(row(1, "a", 10)),
                Op::IngestLake(row(2, "b", 20)),
                Op::IngestWal(wal(2, "b", 25, Update, 2, 1)),
                Op::HoldPin,
            ],
            expect: expect(&[(1, "a", 10), (2, "b", 25)]),
            expect_drained: None,
            expect_offsets: None,
            expect_count: Some(2),
            expect_sum: Some(35),
        },
        Script {
            name: "h_hold_compaction_pin_frozen",
            ops: vec![
                Op::IngestWal(wal(1, "a", 1, Insert, 1, 1)),
                Op::IngestWal(wal(2, "b", 2, Insert, 1, 2)),
                Op::HoldPin,
                Op::CompactThru(2),
                Op::IngestWal(wal(3, "c", 3, Insert, 1, 3)),
            ],
            expect: expect(&[(1, "a", 1), (2, "b", 2)]),
            expect_drained: Some(false),
            expect_offsets: None,
            expect_count: Some(2),
            expect_sum: Some(3),
        },
        Script {
            name: "i_crash_before_commit_offset_unchanged",
            ops: vec![
                Op::IngestWal(wal(1, "first", 1, Insert, 1, 1)),
                Op::Failpoint(FailpointName::BeforeCatalogCommit),
                Op::IngestWal(wal(2, "second", 2, Insert, 1, 2)),
                Op::Reopen,
            ],
            expect: expect(&[(1, "first", 1)]),
            expect_offsets: Some(1),
            expect_drained: None,
            expect_count: Some(1),
            expect_sum: Some(1),
        },
        Script {
            name: "a_data_dir_min_free_pause_resume",
            ops: vec![
                Op::IngestWal(wal(1, "kept", 1, Insert, 1, 1)),
                Op::DiskPause,
                Op::IngestWal(wal(2, "paused", 2, Insert, 1, 2)),
                Op::DiskResume,
                Op::IngestWal(wal(3, "after", 3, Insert, 1, 3)),
            ],
            expect: expect(&[(1, "kept", 1), (3, "after", 3)]),
            expect_drained: Some(false),
            expect_offsets: Some(2),
            expect_count: Some(2),
            expect_sum: Some(4),
        },
        Script {
            name: "a_deadletter_unreadable_json_does_not_advance_offset",
            ops: vec![
                Op::IngestWal(wal(1, "good", 1, Insert, 1, 1)),
                Op::DeadletterSkip,
                Op::IngestWal(wal(2, "later", 2, Insert, 1, 2)),
            ],
            expect: expect(&[(1, "good", 1)]),
            expect_drained: Some(false),
            expect_offsets: Some(1),
            expect_count: Some(1),
            expect_sum: Some(1),
        },
        Script {
            name: "b_stream_tail_wal_registered",
            ops: vec![Op::IngestWal(wal(1, "stream", 7, Insert, 1, 1))],
            expect: expect(&[(1, "stream", 7)]),
            expect_drained: Some(false),
            expect_offsets: None,
            expect_count: Some(1),
            expect_sum: Some(7),
        },
        Script {
            name: "b_hold_compaction_pin_frozen",
            ops: vec![
                Op::IngestWal(wal(1, "a", 1, Insert, 1, 1)),
                Op::IngestWal(wal(2, "b", 2, Insert, 1, 2)),
                Op::HoldPin,
                Op::CompactThru(2),
            ],
            expect: expect(&[(1, "a", 1), (2, "b", 2)]),
            expect_drained: Some(true),
            expect_offsets: None,
            expect_count: Some(2),
            expect_sum: Some(3),
        },
        {
            let n = OVERLAP_N;
            let mut ops = Vec::new();
            for i in 1..=n {
                ops.push(Op::IngestWal(wal(i, "first", i, Insert, 1, i as u64)));
            }
            ops.push(Op::Failpoint(FailpointName::HoldBeforeCompactionSink));
            for i in (n + 1)..=(n * 2) {
                ops.push(Op::IngestWal(wal(i, "second", i, Insert, 1, i as u64)));
            }
            ops.push(Op::CompactThru((n * 2) as u64));
            let expect: BTreeMap<_, _> = (1..=n * 2)
                .map(|i| {
                    let name = if i <= n { "first" } else { "second" };
                    (i, (name.to_string(), i))
                })
                .collect();
            Script {
                name: "b_overlap_ingest_under_hold",
                ops,
                expect,
                expect_drained: Some(false),
                expect_offsets: None,
                expect_count: Some((n * 2) as usize),
                expect_sum: Some((1..=n * 2).sum()),
            }
        },
        Script {
            name: "c_crash_before_catalog_commit",
            ops: vec![
                Op::Failpoint(FailpointName::BeforeCatalogCommit),
                Op::IngestWal(wal(1, "lost", 1, Insert, 1, 1)),
                Op::Reopen,
            ],
            expect: BTreeMap::new(),
            expect_drained: Some(true),
            expect_offsets: Some(0),
            expect_count: Some(0),
            expect_sum: Some(0),
        },
        Script {
            name: "c_after_catalog_before_offsets",
            ops: vec![
                Op::IngestWal(wal(1, "kept", 4, Insert, 1, 1)),
                Op::Failpoint(FailpointName::AfterCatalogCommit),
                Op::Reopen,
            ],
            expect: expect(&[(1, "kept", 4)]),
            expect_drained: None,
            expect_offsets: Some(1),
            expect_count: Some(1),
            expect_sum: Some(4),
        },
        Script {
            name: "c_after_replica_payload_ack_noop",
            ops: vec![
                Op::Failpoint(FailpointName::AfterReplicaPayloadAck),
                Op::IngestWal(wal(1, "replica", 1, Insert, 1, 1)),
            ],
            expect: expect(&[(1, "replica", 1)]),
            expect_drained: Some(false),
            expect_offsets: None,
            expect_count: Some(1),
            expect_sum: Some(1),
        },
        Script {
            name: "c_before_compaction_sink",
            ops: vec![
                Op::IngestWal(wal(1, "a", 1, Insert, 1, 1)),
                Op::Failpoint(FailpointName::HoldBeforeCompactionSink),
                Op::CompactThru(1),
            ],
            expect: expect(&[(1, "a", 1)]),
            expect_drained: Some(false),
            expect_offsets: None,
            expect_count: Some(1),
            expect_sum: Some(1),
        },
        Script {
            name: "c_after_compaction_sink",
            ops: vec![
                Op::IngestWal(wal(1, "a", 1, Insert, 1, 1)),
                Op::Failpoint(FailpointName::AfterCompactionSink),
                Op::CompactThru(1),
            ],
            expect: expect(&[(1, "a", 1)]),
            expect_drained: Some(true),
            expect_offsets: None,
            expect_count: Some(1),
            expect_sum: Some(1),
        },
        Script {
            name: "c_recover_leftover_committed_slices",
            ops: vec![
                Op::IngestWal(wal(1, "ready", 1, Insert, 1, 1)),
                Op::Reopen,
                Op::IngestWal(wal(2, "live", 2, Insert, 1, 2)),
            ],
            expect: expect(&[(1, "ready", 1), (2, "live", 2)]),
            expect_drained: Some(false),
            expect_offsets: Some(2),
            expect_count: Some(2),
            expect_sum: Some(3),
        },
        Script {
            name: "c_dead_sent_gone_dropped",
            ops: vec![
                Op::IngestWal(wal(1, "live", 1, Insert, 1, 1)),
                Op::CompactThru(1),
            ],
            expect: expect(&[(1, "live", 1)]),
            expect_drained: Some(true),
            expect_offsets: None,
            expect_count: Some(1),
            expect_sum: Some(1),
        },
        Script {
            name: "d_copy_grouped_wal_drains",
            ops: vec![
                Op::IngestWal(WalEvent {
                    row: row(1, "copy", 1),
                    mutation: None,
                    order: 0,
                    seq: 1,
                    ordinal: 0,
                    row_i: 0,
                }),
                Op::IngestWal(WalEvent {
                    row: row(2, "copy", 2),
                    mutation: None,
                    order: 0,
                    seq: 2,
                    ordinal: 0,
                    row_i: 0,
                }),
                Op::CompactThru(2),
            ],
            expect: expect(&[(1, "copy", 1), (2, "copy", 2)]),
            expect_drained: Some(true),
            expect_offsets: None,
            expect_count: Some(2),
            expect_sum: Some(3),
        },
        Script {
            name: "g_processor_insert",
            ops: vec![Op::IngestWal(wal(1, "A", 100, Insert, 1, 1))],
            expect: expect(&[(1, "A", 100)]),
            expect_drained: None,
            expect_offsets: None,
            expect_count: Some(1),
            expect_sum: Some(100),
        },
        Script {
            name: "g_processor_update",
            ops: vec![
                Op::IngestWal(wal(1, "A", 100, Insert, 1, 1)),
                Op::IngestWal(wal(1, "A", 150, Update, 2, 2)),
            ],
            expect: expect(&[(1, "A", 150)]),
            expect_drained: None,
            expect_offsets: None,
            expect_count: Some(1),
            expect_sum: Some(150),
        },
        Script {
            name: "g_processor_delete",
            ops: vec![
                Op::IngestWal(wal(1, "A", 80, Insert, 1, 1)),
                Op::IngestWal(wal(1, "A", 80, Delete, 2, 2)),
            ],
            expect: BTreeMap::new(),
            expect_drained: None,
            expect_offsets: None,
            expect_count: Some(0),
            expect_sum: Some(0),
        },
        Script {
            name: "g_processor_snapshot",
            ops: vec![
                Op::IngestLake(row(1, "A", 5)),
                Op::IngestWal(wal(1, "A", 5, Snapshot, 1, 1)),
                Op::IngestWal(wal(2, "B", 7, Insert, 1, 2)),
                Op::IngestWal(wal(2, "B", 11, Update, 2, 3)),
                Op::IngestWal(wal(1, "A", 5, Delete, 2, 4)),
            ],
            expect: expect(&[(2, "B", 11)]),
            expect_drained: None,
            expect_offsets: None,
            expect_count: Some(1),
            expect_sum: Some(11),
        },
        Script {
            name: "g_processor_pk_change",
            ops: vec![
                Op::IngestWal(wal(1, "inv", 25, Insert, 1, 1)),
                Op::IngestWal(wal(1, "inv", 25, Delete, 2, 2)),
                Op::IngestWal(wal(2, "inv", 25, Insert, 3, 3)),
            ],
            expect: expect(&[(2, "inv", 25)]),
            expect_drained: None,
            expect_offsets: None,
            expect_count: Some(1),
            expect_sum: Some(25),
        },
        Script {
            name: "h_stream_overlap_peek",
            ops: vec![
                Op::IngestWal(wal(1, "a", 1, Insert, 1, 1)),
                Op::Failpoint(FailpointName::HoldBeforeCompactionSink),
                Op::IngestWal(wal(2, "b", 2, Insert, 1, 2)),
            ],
            expect: expect(&[(1, "a", 1), (2, "b", 2)]),
            expect_drained: Some(false),
            expect_offsets: None,
            expect_count: Some(2),
            expect_sum: Some(3),
        },
        Script {
            name: "h_lake_only_is_pin_drained",
            ops: vec![
                Op::IngestWal(wal(1, "a", 1, Insert, 1, 1)),
                Op::CompactThru(1),
            ],
            expect: expect(&[(1, "a", 1)]),
            expect_drained: Some(true),
            expect_offsets: None,
            expect_count: Some(1),
            expect_sum: Some(1),
        },
        Script {
            name: "e_value_then_null",
            ops: vec![
                Op::IngestWal(wal(1, "set", 9, Insert, 1, 1)),
                Op::IngestWal(wal(1, "", 9, Update, 2, 2)),
            ],
            expect: expect(&[(1, "", 9)]),
            expect_drained: None,
            expect_offsets: None,
            expect_count: Some(1),
            expect_sum: Some(9),
        },
        Script {
            name: "c_after_offsets_published_crash_reopen",
            ops: vec![
                Op::IngestWal(wal(1, "kept", 1, Insert, 1, 1)),
                Op::Failpoint(FailpointName::AfterOffsetsPublished),
                Op::Reopen,
            ],
            expect: expect(&[(1, "kept", 1)]),
            expect_offsets: Some(1),
            expect_drained: None,
            expect_count: Some(1),
            expect_sum: Some(1),
        },
        Script {
            name: "c_before_compaction_sink_aborts_at_boundary",
            ops: vec![
                Op::IngestWal(wal(1, "live", 1, Insert, 1, 1)),
                Op::Failpoint(FailpointName::BeforeCompactionSink),
                Op::CompactThru(1),
            ],
            expect: expect(&[(1, "live", 1)]),
            expect_drained: Some(false),
            expect_offsets: None,
            expect_count: Some(1),
            expect_sum: Some(1),
        },
        Script {
            name: "c_release_compaction_then_compact",
            ops: vec![
                Op::IngestWal(wal(1, "held", 1, Insert, 1, 1)),
                Op::Failpoint(FailpointName::HoldBeforeCompactionSink),
                Op::CompactThru(1),
                Op::ReleaseCompaction,
                Op::CompactThru(1),
            ],
            expect: expect(&[(1, "held", 1)]),
            expect_drained: Some(true),
            expect_offsets: None,
            expect_count: Some(1),
            expect_sum: Some(1),
        },
        Script {
            name: "h_release_pin_sees_post_compact_wal",
            ops: vec![
                Op::IngestWal(wal(1, "pinned", 1, Insert, 1, 1)),
                Op::HoldPin,
                Op::CompactThru(1),
                Op::ReleasePin,
                Op::IngestWal(wal(2, "after", 2, Insert, 1, 2)),
            ],
            expect: expect(&[(1, "pinned", 1), (2, "after", 2)]),
            expect_drained: Some(false),
            expect_offsets: None,
            expect_count: Some(2),
            expect_sum: Some(3),
        },
        Script {
            name: "b_stable_key_scoped_stale_per_business_key",
            ops: vec![
                Op::IngestWal(wal(1, "alpha", 10, Insert, 5, 1)),
                Op::IngestWal(wal(2, "beta", 20, Insert, 5, 2)),
                Op::IngestWal(wal(2, "stale", 1, Update, 1, 3)),
            ],
            expect: expect(&[(1, "alpha", 10), (2, "beta", 20)]),
            expect_drained: None,
            expect_offsets: None,
            expect_count: Some(2),
            expect_sum: Some(30),
        },
        Script {
            name: "h_group_by_on_pin",
            ops: vec![
                Op::IngestWal(wal(1, "a", 10, Insert, 1, 1)),
                Op::IngestWal(wal(2, "b", 20, Insert, 1, 2)),
                Op::HoldPin,
            ],
            expect: expect(&[(1, "a", 10), (2, "b", 20)]),
            expect_drained: Some(false),
            expect_offsets: None,
            expect_count: Some(2),
            expect_sum: Some(30),
        },
    ]
}

/// Production-API fixtures that are not World schedules. Each name has a
/// `#[test]` in a sibling dst module.
pub const EXTRA_FIXTURES: &[&str] = &[
    "a_tiny_slices_wait_group_bytes",
    "a_group_bytes_compact_without_time",
    "a_flatten_on_vs_off_field_ids_stable",
    "a_compaction_time_threshold_readiness",
    "a_slice_oversize_planned_alone",
    "a_group_bytes_do_not_cross_namespace",
    "a_per_sink_namespace_hard_cap_holds",
    "a_slice_bitmap_partial_done_blocks_reclaim",
    "a_compaction_sent_fence",
    "a_orphan_gc",
    "a_quarantine_dir_without_handle",
    "a_payload_unreadable",
    "b_composite_business_keys",
    "b_mixed_namespaces_yaml_cdc_keys",
    "b_missing_cdc_keys_fail_closed",
    "b_unsupported_order_rejected_for_exact_once",
    "b_reclaim_between_getflight_and_doget_wal_pin_lost",
    "b_wal_null_business_key_fail_closed",
    "b_unknown_mutation_sidecar_fail_closed",
    "b_cdc_rows_on_append_rejected",
    "b_projection_without_pk_fail_closed",
    "c_putcond_not_exists_same_sha",
    "c_stale_fence_rejected",
    "c_unreadable_compaction_json_fail_closed",
    "c_unreadable_compaction_json_names_file",
    "c_import_leftover_once",
    "c_pagination_and_delete_prefix",
    "c_legacy_slice_put_rejected",
    "c_occ_concurrent_reclaim",
    "c_reclaim_vs_stream_processor_cursor",
    "c_putcond_version_mismatch",
    "c_deletecond_version_mismatch",
    "c_format_imported_resume",
    "c_pending_to_sent",
    "c_sent_vs_acked",
    "c_after_compaction_sink_leaves_sent",
    "c_sent_hydrate",
    "c_redo_acked",
    "c_persist_matching_sha_skip",
    "c_hydrate_without_segf",
    "c_failpoint_file_is_single_shot",
    "c_complete_slices_and_reclaim",
    "d_handshake_prepare_sink_rawbytes",
    "d_sink_pool_does_not_hold_notify_across_grow",
    "d_copy_grouped_timeout_is_900s",
    "d_trailing_bincode_rejected",
    "d_protocol_22_pipeline_type",
    "d_installed_four_byte_unit",
    "d_copy_cannot_set_flatten_or_aggregates",
    "d_host_frame_run_source_roundtrip",
    "d_install_schema_delta",
    "d_schema_state_refresh_required",
    "d_offset_request_rpc",
    "d_grouped_compaction_phase",
    "e_type_widen_legal_illegal_promotion",
    "e_nested_object_array_json",
    "e_repair_duplicate_zero_field_ids",
    "e_rename_via_evolution_preserves_field_id",
    "e_new_namespace_mid_pipeline",
    "e_discover_never_panics_then_query",
    "e_exact_arrow_eligible",
    "e_exact_arrow_flatten_fallback",
    "e_exact_arrow_nested_ineligible",
    "f_rename_column",
    "f_rename_column_via_surviving_name",
    "f_rename_dotted_creates_parent",
    "f_flattened_and_dotted_resolve",
    "f_hyphenated_quoted_namespace",
    "f_merge_column",
    "f_drop_column",
    "f_drop_column_absent_from_current_table",
    "f_alter_column_type_widen",
    "f_illegal_type_fail_closed",
    "f_journal_idempotent_retry",
    "f_journal_checksum_mismatch",
    "f_query_sql_alter_journals_query_checksum",
    "f_down_only_rename_promote",
    "f_down_drop_merge_fail_closed",
    "f_glue_hive_clickhouse_alter_fail_closed",
    "f_query_after_migrate_two_part_name",
    "f_iceberg_replace_schema_rename_field_id",
    "f_migrate_reuses_open_store",
    "g_crash_before_txn",
    "g_crash_before_commit",
    "g_crash_during_txn",
    "g_crash_after_txn",
    "g_config_hash_mismatch_rebuilds",
    "g_reclaim_hold_restore_from_disk_cursor",
    "g_processor_dedup_event_id",
    "g_filter_keeps_cdc_sidecar",
    "g_transform_error_does_not_advance_offsets",
    "g_multi_row_batch_atomicity",
    "g_clustered_wal_rejects_aggregates",
    "g_copy_cannot_set_ops_aggregates",
    "g_running_sum_count",
    "g_cdc_update_delta",
    "g_delete_subtract",
    "g_missing_last_row_as_insert",
    "g_pk_change_pending_index",
    "g_already_applied_slice_skip",
    "g_filter_project_derive_compose",
    "g_unknown_project",
    "g_schema_change_derived_column",
    "h_unknown_pipeline_fail_closed",
    "h_oversized_sql_fail_closed",
    "h_query_pin_retry_ceiling",
    "h_foreign_tenant_ticket",
    "h_minus_excluded",
    "h_clustered_pin_union",
    "h_illegal_sql",
    "h_lake_only_env_is_deleted",
    "i_activate_ingest_lease_then_persist",
    "i_exclusive_sled_open_fail_closed",
    "i_lease_epoch_stale_writer_rejected",
    "i_checkpoint_offset_hint_roundtrip",
    "i_offset_position_filesize_closed",
    "i_offset_corrupt_schema",
    "j_cube_events_namespaces",
    "j_device_json_wide_row",
    "j_s3_list_closed_skip",
    "j_debezium_op_maps_to_mutation_kind",
];

pub const REQUIRED_PLAN_SCRIPTS: &[&str] = &[
    "a_empty_lake_insert_count",
    "a_two_namespaces_as_two_keys",
    "a_compact_first_day_keeps_second",
    "a_tiny_slices_wait_group_bytes",
    "a_group_bytes_compact_without_time",
    "a_flatten_on_vs_off_field_ids_stable",
    "a_compaction_time_threshold_readiness",
    "a_slice_oversize_planned_alone",
    "a_group_bytes_do_not_cross_namespace",
    "a_per_sink_namespace_hard_cap_holds",
    "a_slice_bitmap_partial_done_blocks_reclaim",
    "a_compaction_sent_fence",
    "a_orphan_gc",
    "a_quarantine_dir_without_handle",
    "a_payload_unreadable",
    "a_duplicate_payload_sha_does_not_rewrite",
    "a_crash_before_catalog_drops_ingest",
    "a_crash_after_catalog_republishes_offsets",
    "a_data_dir_min_free_pause_resume",
    "a_deadletter_unreadable_json_does_not_advance_offset",
    "b_uncompacted_snapshot_replaces_lake",
    "b_snapshot_fills_empty_lake",
    "b_insert_unions_with_lake",
    "b_update_replaces_lake_row",
    "b_delete_removes_key",
    "b_many_updates_keep_max_order_token",
    "b_equal_tokens_tie_break_seq",
    "b_update_then_delete_absent",
    "b_delete_then_reinsert",
    "b_compacted_snapshot_leftover_does_not_replace_lake",
    "b_pk_change_delete_old_insert_new",
    "b_composite_business_keys",
    "b_stale_wal_token_loses_to_newer",
    "b_mixed_namespaces_yaml_cdc_keys",
    "b_missing_cdc_keys_fail_closed",
    "b_unsupported_order_rejected_for_exact_once",
    "b_append_does_not_suppress_lake",
    "b_compact_prefix_then_live_update",
    "b_hold_compaction_pin_frozen",
    "b_stream_tail_wal_registered",
    "b_overlap_ingest_under_hold",
    "b_point_id_1_after_mix",
    "b_reclaim_between_getflight_and_doget_wal_pin_lost",
    "b_wal_null_business_key_fail_closed",
    "b_unknown_mutation_sidecar_fail_closed",
    "b_cdc_rows_on_append_rejected",
    "b_projection_without_pk_fail_closed",
    "b_stable_key_scoped_stale_per_business_key",
    "c_putcond_not_exists_same_sha",
    "c_stale_fence_rejected",
    "c_crash_before_catalog_commit",
    "c_after_catalog_before_offsets",
    "c_after_offsets_published_idempotent_reopen",
    "c_after_replica_payload_ack_noop",
    "c_before_compaction_sink",
    "c_after_compaction_sink",
    "c_recover_leftover_committed_slices",
    "c_compaction_acked_is_terminal",
    "c_dead_sent_gone_dropped",
    "c_import_leftover_once",
    "c_pagination_and_delete_prefix",
    "c_legacy_slice_put_rejected",
    "c_unreadable_compaction_json_fail_closed",
    "c_unreadable_compaction_json_names_file",
    "c_occ_concurrent_reclaim",
    "c_reclaim_vs_stream_processor_cursor",
    "c_putcond_version_mismatch",
    "c_deletecond_version_mismatch",
    "c_format_imported_resume",
    "c_pending_to_sent",
    "c_sent_vs_acked",
    "c_after_compaction_sink_leaves_sent",
    "c_sent_hydrate",
    "c_redo_acked",
    "c_persist_matching_sha_skip",
    "c_hydrate_without_segf",
    "c_after_offsets_published_crash_reopen",
    "c_before_compaction_sink_aborts_at_boundary",
    "c_release_compaction_then_compact",
    "c_failpoint_file_is_single_shot",
    "c_complete_slices_and_reclaim",
    "d_handshake_prepare_sink_rawbytes",
    "d_trailing_bincode_rejected",
    "d_protocol_22_pipeline_type",
    "d_namespace_burst_then_identity_put",
    "d_installed_four_byte_unit",
    "d_copy_grouped_wal_drains",
    "d_copy_cannot_set_flatten_or_aggregates",
    "d_sink_pool_does_not_hold_notify_across_grow",
    "d_copy_grouped_timeout_is_900s",
    "d_copy_identity_append_does_not_cdc_merge",
    "d_host_frame_run_source_roundtrip",
    "d_install_schema_delta",
    "d_schema_state_refresh_required",
    "d_offset_request_rpc",
    "d_grouped_compaction_phase",
    "e_add_leaf_old_rows_still_query",
    "e_type_widen_legal_illegal_promotion",
    "e_null_then_value",
    "e_value_then_null",
    "e_nested_object_array_json",
    "e_repair_duplicate_zero_field_ids",
    "e_rename_via_evolution_preserves_field_id",
    "e_new_namespace_mid_pipeline",
    "e_discover_never_panics_then_query",
    "e_exact_arrow_eligible",
    "e_exact_arrow_flatten_fallback",
    "e_exact_arrow_nested_ineligible",
    "f_rename_column",
    "f_rename_column_via_surviving_name",
    "f_rename_dotted_creates_parent",
    "f_flattened_and_dotted_resolve",
    "f_hyphenated_quoted_namespace",
    "f_merge_column",
    "f_drop_column",
    "f_drop_column_absent_from_current_table",
    "f_alter_column_type_widen",
    "f_illegal_type_fail_closed",
    "f_journal_idempotent_retry",
    "f_journal_checksum_mismatch",
    "f_query_sql_alter_journals_query_checksum",
    "f_down_only_rename_promote",
    "f_down_drop_merge_fail_closed",
    "f_glue_hive_clickhouse_alter_fail_closed",
    "f_query_after_migrate_two_part_name",
    "f_iceberg_replace_schema_rename_field_id",
    "f_migrate_reuses_open_store",
    "g_processor_insert",
    "g_processor_update",
    "g_processor_delete",
    "g_processor_snapshot",
    "g_processor_pk_change",
    "g_processor_dedup_event_id",
    "g_crash_before_txn",
    "g_crash_before_commit",
    "g_crash_during_txn",
    "g_crash_after_txn",
    "g_config_hash_mismatch_rebuilds",
    "g_reclaim_hold_restore_from_disk_cursor",
    "g_multi_row_batch_atomicity",
    "g_clustered_wal_rejects_aggregates",
    "g_copy_cannot_set_ops_aggregates",
    "g_filter_keeps_cdc_sidecar",
    "g_transform_error_does_not_advance_offsets",
    "g_running_sum_count",
    "g_cdc_update_delta",
    "g_delete_subtract",
    "g_missing_last_row_as_insert",
    "g_pk_change_pending_index",
    "g_already_applied_slice_skip",
    "g_filter_project_derive_compose",
    "g_unknown_project",
    "g_schema_change_derived_column",
    "h_count_sum_point_on_pin",
    "h_group_by_on_pin",
    "h_stream_overlap_peek",
    "h_hold_compaction_pin_frozen",
    "h_query_after_compact_is_drained",
    "h_lake_only_is_pin_drained",
    "h_unknown_pipeline_fail_closed",
    "h_oversized_sql_fail_closed",
    "h_query_pin_retry_ceiling",
    "h_foreign_tenant_ticket",
    "h_minus_excluded",
    "h_clustered_pin_union",
    "h_illegal_sql",
    "h_lake_only_env_is_deleted",
    "h_release_pin_sees_post_compact_wal",
    "i_crash_before_commit_offset_unchanged",
    "i_activate_ingest_lease_then_persist",
    "i_exclusive_sled_open_fail_closed",
    "i_lease_epoch_stale_writer_rejected",
    "i_checkpoint_offset_hint_roundtrip",
    "i_offset_position_filesize_closed",
    "i_offset_corrupt_schema",
    "j_cube_events_namespaces",
    "j_device_json_wide_row",
    "j_s3_list_closed_skip",
    "j_debezium_op_maps_to_mutation_kind",
];

#[test]
fn every_named_script_matches_production_current_table() {
    for script in all_scripts() {
        let world = run(&script.ops);
        let got = current_table(&world);
        assert_eq!(got, script.expect, "script {}", script.name);
        if let Some(drained) = script.expect_drained {
            assert_eq!(
                pin_drained(&world),
                drained,
                "script {} wal_pin",
                script.name
            );
        }
        if let Some(offsets) = script.expect_offsets {
            assert_eq!(world.offsets, offsets, "script {} offsets", script.name);
        }
        if let Some(n) = script.expect_count {
            assert_eq!(count(&world), n, "script {} count", script.name);
        }
        if let Some(sum) = script.expect_sum {
            assert_eq!(sum_amount(&world), sum, "script {} sum", script.name);
        }
    }
}

#[test]
fn script_catalog_is_complete() {
    let mut names: BTreeSet<_> = all_scripts().into_iter().map(|s| s.name).collect();
    names.extend(EXTRA_FIXTURES.iter().copied());
    for required in REQUIRED_PLAN_SCRIPTS {
        assert!(names.contains(required), "missing plan fixture {required}");
    }
    let present: BTreeSet<_> = names
        .iter()
        .map(|n| DstGroup::from_script_name(n))
        .collect();
    for group in DstGroup::ALL {
        assert!(present.contains(&group), "missing DST group {group:?}");
    }
}

#[test]
fn fuzz_corpus_has_every_named_script() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz/corpus/world_schedule");
    std::fs::create_dir_all(&root).unwrap();
    let wanted: BTreeSet<_> = all_scripts()
        .into_iter()
        .map(|s| s.name.to_string())
        .collect();
    for script in all_scripts() {
        let path = root.join(script.name);
        let bytes = encode_ops(&script.ops);
        let existing = std::fs::read(&path).unwrap_or_default();
        if existing != bytes {
            std::fs::write(&path, &bytes).unwrap();
        }
        assert!(
            path.exists(),
            "missing world_schedule corpus {}",
            script.name
        );
        if script.ops.len() > 256 {
            continue;
        }
        let decoded = schedule_from_bytes(&std::fs::read(&path).unwrap());
        assert_eq!(
            current_table(&run(&decoded)),
            current_table(&run(&script.ops)),
            "corpus round-trip {}",
            script.name
        );
    }
    let crash = root.join("crash_0b179261");
    assert!(
        crash.is_file(),
        "missing world_schedule crash corpus crash_0b179261"
    );
    assert!(
        schedule_from_bytes(&std::fs::read(&crash).unwrap()).is_empty(),
        "crash_0b179261 must fail closed"
    );
    for entry in std::fs::read_dir(&root).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if !wanted.contains(name.as_str()) && !name.starts_with("crash_") {
            std::fs::remove_file(&path).unwrap();
        }
    }
    assert!(crash.is_file(), "janitor must keep crash_0b179261");
}

#[test]
fn compacted_snapshot_leftover_stays_in_catalog_and_does_not_anti_join() {
    let world = run(&[
        Op::IngestLake(row(1, "alpha", 120)),
        Op::CompactThru(1),
        Op::IngestWal(wal(1, "alpha", 100, MutationKind::Snapshot, 1, 1)),
    ]);
    assert_eq!(
        catalog_segments(&world).len(),
        1,
        "leftover WAL row remains"
    );
    assert!(pin_drained(&world), "seq<=E is not live WAL");
    assert_eq!(current_table(&world), expect(&[(1, "alpha", 120)]));
}

#[test]
fn world_ingest_commits_catalog_and_payload() {
    let world = run(&[Op::IngestWal(wal(1, "a", 10, MutationKind::Insert, 1, 1))]);
    assert_eq!(catalog_segments(&world).len(), 1);
    assert_eq!(world.payloads.len(), 1);
    assert_eq!(world.offsets, 1);
}

#[test]
fn world_failpoint_before_catalog_leaves_no_segment() {
    let world = run(&[
        Op::Failpoint(FailpointName::BeforeCatalogCommit),
        Op::IngestWal(wal(1, "lost", 1, MutationKind::Insert, 1, 1)),
    ]);
    assert!(catalog_segments(&world).is_empty());
    assert!(world.payloads.is_empty());
}

#[test]
fn world_compact_appends_iceberg_snapshot_and_acked_compaction() {
    let world = run(&[
        Op::IngestWal(wal(1, "a", 1, MutationKind::Insert, 1, 1)),
        Op::CompactThru(1),
    ]);
    let snap = world.snapshots.last().expect("iceberg snapshot");
    assert_eq!(snap.compacted_thru, 1);
    assert_eq!(snap.rows.len(), 1);
    let rows = world
        .catalog
        .query_prefix(CatalogPrefix::Compaction, None, 8)
        .unwrap();
    assert!(
        rows.iter().any(|(_, row)| matches!(
            row,
            CatalogRow::Compaction(c) if c.state == CompactionState::Acked
        )),
        "{rows:?}"
    );
}

#[test]
fn world_hold_pin_records_sew() {
    let world = run(&[
        Op::IngestWal(wal(1, "a", 1, MutationKind::Insert, 1, 1)),
        Op::CompactThru(1),
        Op::IngestWal(wal(2, "b", 2, MutationKind::Insert, 1, 2)),
        Op::HoldPin,
    ]);
    let pin = world.held_pin.as_ref().expect("query pin");
    assert!(matches!(pin.lake, LakeSnapshot::At(_)));
    assert_eq!(pin.compacted_thru, 1);
    assert_eq!(pin.wal.len(), 1);
}

#[test]
fn e_add_leaf_records_stable_field_id() {
    let world = run(&[
        Op::IngestLake(row(1, "before", 1)),
        Op::EvolveAddColumn("leaf".into()),
        Op::IngestWal(wal(2, "after", 2, MutationKind::Insert, 1, 1)),
    ]);
    assert!(world.field_ids.contains_key("leaf"));
    assert_eq!(current_table(&world).len(), 2);
}

#[test]
fn world_duplicate_payload_sha_does_not_rewrite() {
    let world = run(&[
        Op::IngestWal(wal(1, "same", 1, MutationKind::Insert, 1, 1)),
        Op::IngestWal(wal(1, "same", 1, MutationKind::Insert, 1, 2)),
    ]);
    assert_eq!(world.payloads.len(), 1);
    assert_eq!(catalog_segments(&world).len(), 1);
}

#[test]
fn extra_fixtures_have_named_tests() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/sqlrt/dst");
    let mut src = String::new();
    for entry in std::fs::read_dir(&root).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            src.push_str(&std::fs::read_to_string(&path).unwrap());
        }
    }
    for name in EXTRA_FIXTURES {
        let needle = format!("fn {name}(");
        assert!(src.contains(&needle), "missing #[test] {needle}");
    }
}
