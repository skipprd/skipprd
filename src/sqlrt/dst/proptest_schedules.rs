use super::*;
use crate::plugins::cdc::MutationKind;
use proptest::prelude::*;

const DST_CASES: u32 = 32;

fn arb_mutation() -> impl Strategy<Value = MutationKind> {
    prop_oneof![
        Just(MutationKind::Snapshot),
        Just(MutationKind::Insert),
        Just(MutationKind::Update),
        Just(MutationKind::Delete),
    ]
}

fn arb_wal() -> impl Strategy<Value = WalEvent> {
    (
        1i64..6,
        0i64..40,
        arb_mutation(),
        0u8..10,
        1u64..8,
        0u32..3,
        0u64..3,
    )
        .prop_map(
            |(id, amount, mutation, order, seq, ordinal, row_i)| WalEvent {
                row: UserRow {
                    id,
                    name: format!("k{id}"),
                    amount,
                },
                mutation: Some(mutation),
                order,
                seq,
                ordinal,
                row_i,
            },
        )
}

fn arb_op() -> impl Strategy<Value = Op> {
    use crate::failpoint::FailpointName;
    prop_oneof![
        (1i64..6, 0i64..40).prop_map(|(id, amount)| Op::IngestLake(UserRow {
            id,
            name: format!("l{id}"),
            amount,
        })),
        arb_wal().prop_map(Op::IngestWal),
        (0u64..8).prop_map(Op::CompactThru),
        Just(Op::Failpoint(FailpointName::AfterCatalogCommit)),
        Just(Op::Failpoint(FailpointName::BeforeCatalogCommit)),
        Just(Op::Failpoint(FailpointName::AfterOffsetsPublished)),
        Just(Op::Failpoint(FailpointName::AfterCompactionSink)),
        Just(Op::Failpoint(FailpointName::BeforeCompactionSink)),
        Just(Op::Failpoint(FailpointName::HoldBeforeCompactionSink)),
        Just(Op::Failpoint(FailpointName::AfterReplicaPayloadAck)),
        Just(Op::Reopen),
        Just(Op::HoldPin),
        Just(Op::ReleasePin),
        Just(Op::ReleaseCompaction),
        Just(Op::DiskPause),
        Just(Op::DiskResume),
        Just(Op::DeadletterSkip),
        Just(Op::EvolveAddColumn("extra".into())),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(DST_CASES))]

    #[test]
    fn random_schedules_do_not_panic_and_are_deterministic(ops in proptest::collection::vec(arb_op(), 1..12)) {
        let mut world = World::new();
        for op in &ops {
            apply(&mut world, op.clone());
            if matches!(op, Op::CompactThru(_)) {
                let table = current_table(&world);
                prop_assert_eq!(count(&world), table.len());
                prop_assert_eq!(
                    sum_amount(&world),
                    table.values().map(|(_, amount)| amount).sum::<i64>()
                );
                if let Some(id) = table.keys().next().copied() {
                    prop_assert!(table.contains_key(&id));
                }
            }
        }
        let a = world;
        let b = run(&ops);
        prop_assert_eq!(current_table(&a), current_table(&b));
        prop_assert_eq!(count(&a), count(&b));
        prop_assert_eq!(sum_amount(&a), sum_amount(&b));
        let _ = pin_drained(&a);
        let _ = compacted_seqs(&a);
    }

    #[test]
    fn schedule_from_bytes_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
        let ops = schedule_from_bytes(&bytes);
        let world = run(&ops);
        let _ = current_table(&world);
        let _ = count(&world);
        let _ = sum_amount(&world);
    }

    #[test]
    fn legal_and_illegal_alter_sql_never_panic(sql in proptest::sample::select(vec![
        "ALTER TABLE s3_alter.s3_alter RENAME COLUMN a TO b",
        "ALTER TABLE bikehire.rides RENAME COLUMN region TO detail.region",
        "ALTER TABLE bikehire.rides MERGE COLUMN price_string INTO price",
        "ALTER TABLE bikehire.rides DROP COLUMN leftover",
        "ALTER TABLE bikehire.rides ALTER COLUMN n TYPE BIGINT",
        "ALTER TABLE s3_alter DROP COLUMN",
        "not sql",
    ])) {
        if let Ok(mut parser) = crate::sqlrt::parser::SParser::new(sql) {
            let _ = parser.parse_statement();
        }
    }

    #[test]
    fn field_ids_are_stable(ns in "[a-z]{1,8}", field in "[a-z]{1,8}") {
        let a = crate::lineage::deterministic_field_id(&ns, &[field.clone()]);
        let b = crate::lineage::deterministic_field_id(&ns, &[field]);
        prop_assert_eq!(a, b);
        prop_assert!(a >= 0);
    }

    #[test]
    fn catalog_row_bincode_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
        let mut txn = skippr_wal_catalog::CatalogTxn::new();
        if let Ok(row) = bincode::deserialize::<skippr_wal_catalog::CatalogRow>(&bytes) {
            let _ = txn.put(
                skippr_wal_catalog::CatalogKey::Format,
                row,
                skippr_wal_catalog::PutCond::NotExists,
            );
        }
    }

    #[test]
    fn copy_frame_bytes_never_parse_tag6_as_length(
        bytes in proptest::collection::vec(any::<u8>(), 0..32)
    ) {
        let _ = crate::runtime_plugins::wire::decode_payload::<
            crate::runtime_plugins::protocol::HostFrame,
        >(&bytes);
        let _ = crate::runtime_plugins::wire::decode_payload::<
            crate::runtime_plugins::protocol::PluginFrame,
        >(&bytes);
    }

    #[test]
    fn evolution_add_column_keeps_prior_rows(
        event in arb_wal(),
        field in "[a-z]{1,8}"
    ) {
        let later = event.clone();
        let ops = vec![
            Op::IngestWal(event),
            Op::EvolveAddColumn(field.clone()),
            Op::IngestWal(WalEvent {
                seq: later.seq + 8,
                ..later
            }),
        ];
        let world = run(&ops);
        prop_assert!(world.field_ids.contains_key(&field));
        let _ = current_table(&world);
        let _ = count(&world);
    }

    #[test]
    fn catalog_txn_seq_advance_and_stale_fence(seq in 1u64..32) {
        use skippr_lease::{LeaseEpoch, NodeId};
        use skippr_wal_catalog::{
            CatalogError, CatalogFence, CatalogKey, CatalogRow, CatalogSeq, CatalogTxn,
            MemoryWalCatalog, PutCond, SeqRow, WalCatalogStore,
        };
        let store = MemoryWalCatalog::new();
        let owner = NodeId::generate();
        store.set_writer_fence(owner, LeaseEpoch::new(1));
        let fence = CatalogFence {
            owner,
            epoch: LeaseEpoch::new(1),
        };
        let mut txn = CatalogTxn::new();
        txn.put(
            CatalogKey::Seq,
            CatalogRow::Seq(SeqRow {
                max_seq: CatalogSeq::new(seq),
            }),
            PutCond::SeqAdvance,
        )
        .unwrap();
        prop_assert!(store.commit(&txn, fence).is_ok());
        let mut again = CatalogTxn::new();
        again
            .put(
                CatalogKey::Seq,
                CatalogRow::Seq(SeqRow {
                    max_seq: CatalogSeq::new(seq),
                }),
                PutCond::SeqAdvance,
            )
            .unwrap();
        prop_assert_eq!(
            store.commit(&again, fence).unwrap_err(),
            CatalogError::ConditionFailed
        );
        let stale = CatalogFence {
            owner,
            epoch: LeaseEpoch::new(9),
        };
        let mut third = CatalogTxn::new();
        third
            .put(
                CatalogKey::Seq,
                CatalogRow::Seq(SeqRow {
                    max_seq: CatalogSeq::new(seq + 1),
                }),
                PutCond::SeqAdvance,
            )
            .unwrap();
        prop_assert_eq!(store.commit(&third, stale).unwrap_err(), CatalogError::Fenced);
    }

    #[test]
    fn ingest_commits_catalog_segments(event in arb_wal()) {
        let world = run(&[Op::IngestWal(event)]);
        prop_assert_eq!(catalog_segments(&world).len(), 1);
        prop_assert_eq!(world.payloads.len(), 1);
    }
}
