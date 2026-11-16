use crate::plugins::cdc::MutationKind;
use crate::processor::test_support::{insert_meta, invoice_batch, DiskFixture, SimEvent};
use crate::processor::{
    allows_reclaim, reset_reclaim_hold, set_min_unconsumed_seq, CrashPoint, WalCursor,
};
use std::collections::BTreeMap;

#[test]
fn c_reclaim_vs_stream_processor_cursor() {
    set_min_unconsumed_seq("events", 5);
    assert!(
        !allows_reclaim("events", 5),
        "must not delete seq still held"
    );
    assert!(allows_reclaim("events", 4));
    set_min_unconsumed_seq("events", u64::MAX);
}

#[test]
fn g_multi_row_batch_atomicity() {
    let fixture = DiskFixture::new();
    let a = SimEvent::new(MutationKind::Insert, "A", 7, "i1", "e1");
    let b = SimEvent::new(MutationKind::Insert, "B", 3, "i2", "e2");
    fixture.apply_all(&[a, b]);
    assert_eq!(
        fixture
            .measure("customer_invoice_totals", "A", "invoice_total")
            .unwrap(),
        7.0
    );
    assert_eq!(
        fixture
            .measure("customer_invoice_totals", "B", "invoice_total")
            .unwrap(),
        3.0
    );
}

#[test]
fn g_incompatible_ir_wipe_clears_measures() {
    let fixture = DiskFixture::new();
    let spec = fixture.spec.clone();
    let batch = invoice_batch(&["A"], &[9], &["inv"]);
    fixture
        .apply_events(
            &spec,
            WalCursor::default(),
            &batch,
            Some(&[insert_meta("e1")]),
            0,
        )
        .unwrap();
    assert_eq!(
        fixture
            .measure("customer_invoice_totals", "A", "invoice_total")
            .unwrap(),
        9.0
    );
    fixture.wipe_compute("customer_invoice_totals");
    assert_eq!(
        fixture
            .measure("customer_invoice_totals", "A", "invoice_total")
            .unwrap(),
        0.0
    );
}

#[test]
fn g_dedup_keeps_higher_token() {
    let fixture = DiskFixture::new();
    let batch = invoice_batch(&["A", "A"], &[10, 20], &["inv", "inv"]);
    let low = crate::plugins::cdc::WalRowMeta::new(MutationKind::Insert, b"same".to_vec(), vec![1]);
    let high =
        crate::plugins::cdc::WalRowMeta::new(MutationKind::Insert, b"same".to_vec(), vec![9]);
    fixture
        .apply_events(
            &fixture.spec,
            WalCursor::default(),
            &batch,
            Some(&[low.clone(), high.clone()]),
            0,
        )
        .unwrap();
    assert_eq!(
        fixture
            .measure("customer_invoice_totals", "A", "invoice_total")
            .unwrap(),
        20.0
    );
    let reverse = invoice_batch(&["A"], &[1], &["inv"]);
    fixture
        .apply_events(
            &fixture.spec,
            WalCursor {
                seq: 1,
                ..WalCursor::default()
            },
            &reverse,
            Some(&[low]),
            0,
        )
        .unwrap();
    assert_eq!(
        fixture
            .measure("customer_invoice_totals", "A", "invoice_total")
            .unwrap(),
        20.0
    );
}

#[test]
fn g_processor_dedup_event_id() {
    let fixture = DiskFixture::new();
    let first = SimEvent::new(MutationKind::Insert, "A", 40, "inv", "same");
    let dup = SimEvent::new(MutationKind::Insert, "A", 40, "inv", "same");
    let mut cursor = WalCursor::default();
    cursor = fixture.apply(&first, cursor).unwrap();
    let _ = fixture.apply(&dup, cursor).unwrap();
    assert_eq!(
        fixture
            .measure("customer_invoice_totals", "A", "invoice_total")
            .unwrap(),
        40.0
    );
}

fn crash_then_reopen(point: CrashPoint, expect_committed: bool) {
    let event = SimEvent::new(MutationKind::Insert, "A", 7, "inv", "e1");
    let (fixture, panicked) = DiskFixture::new().crash_one(&event, WalCursor::default(), point);
    assert!(panicked, "{point:?} must unwind");
    let got = fixture
        .measure("customer_invoice_totals", "A", "invoice_total")
        .unwrap();
    if expect_committed {
        assert_eq!(got, 7.0, "{point:?}");
    } else {
        assert_eq!(got, 0.0, "{point:?}");
    }
}

#[test]
fn g_crash_before_txn() {
    crash_then_reopen(CrashPoint::BeforeTxn, false);
}

#[test]
fn g_crash_before_commit() {
    crash_then_reopen(CrashPoint::BeforeCommit, false);
}

#[test]
fn g_crash_during_txn() {
    crash_then_reopen(CrashPoint::DuringTxn, false);
}

#[test]
fn g_crash_after_txn() {
    crash_then_reopen(CrashPoint::AfterTxn, true);
}

#[test]
fn g_filter_keeps_cdc_sidecar() {
    use crate::compute::{apply_stateless, StatelessSelect};
    use crate::plugins::cdc::WalRowMeta;
    use crate::processor::test_support::invoice_batch;
    let batch = invoice_batch(&["a", "b", "c"], &[0, 5, 0], &["i1", "i2", "i3"]);
    let cdc = vec![
        WalRowMeta::new(MutationKind::Insert, b"a".to_vec(), vec![1]),
        WalRowMeta::new(MutationKind::Insert, b"b".to_vec(), vec![2]),
        WalRowMeta::new(MutationKind::Insert, b"c".to_vec(), vec![3]),
    ];
    let select = StatelessSelect {
        filter: Some("amount > 0".into()),
        derive: Vec::new(),
    };
    let aligned = apply_stateless(&[], Some(&select), batch, Some(cdc)).unwrap();
    assert_eq!(aligned.batch.num_rows(), 1);
    let cdc = aligned.cdc.unwrap();
    assert_eq!(cdc.len(), 1);
    assert_eq!(cdc[0].event_id, b"b");
}

#[test]
fn g_transform_error_does_not_advance_offsets() {
    use crate::helpers::offset_store::{
        apply_offset_field, memory_offset_store, OffsetStore, StoreOffsetStore,
    };
    use crate::helpers::offsets::OffsetTypes;
    use sha2::{Digest, Sha256};
    use skippr_lease::PublishMode;
    let select = crate::compute::StatelessSelect {
        filter: Some("not_a_column > 0".into()),
        derive: Vec::new(),
    };
    let batch = invoice_batch(&["a"], &[1], &["i1"]);
    let store = crate::helpers::offset_store::memory_offset_store();
    let sk = StoreOffsetStore::offset_key("ns", "p");
    let before = apply_offset_field(None, OffsetTypes::Position, 9, false).unwrap();
    let sha = hex::encode(Sha256::digest(&before));
    store
        .publish_wal_commit(&sk, &before, 1, 1, &sha, PublishMode::Fresh)
        .unwrap();
    assert!(crate::compute::apply_stateless(&[], Some(&select), batch, None).is_err());
    assert_eq!(
        store.get_bytes(&sk).unwrap().as_deref(),
        Some(before.as_slice())
    );
}

#[test]
fn g_reclaim_hold_restore_from_disk_cursor() {
    reset_reclaim_hold();
    let fixture = DiskFixture::new();
    let event = SimEvent::new(MutationKind::Insert, "A", 1, "inv", "e1");
    let mut cursor = WalCursor {
        seq: 4,
        segment_id: "seg-4".into(),
        slice_ordinal: 0,
        row_index: 0,
    };
    cursor = fixture.apply(&event, cursor).unwrap();
    assert_eq!(cursor.seq, 4);
    assert!(!allows_reclaim("events", 4));
    assert!(allows_reclaim("events", 3));
    let fixture = fixture.reopen();
    let meta = fixture
        .load_meta("customer_invoice_totals")
        .unwrap()
        .unwrap();
    assert_eq!(meta.cursor.seq, 4);
    assert!(!allows_reclaim("events", 4));
    reset_reclaim_hold();
}

#[test]
fn g_running_sum_count() {
    let fixture = DiskFixture::new();
    fixture
        .apply(
            &SimEvent::new(MutationKind::Insert, "A", 10, "inv-1", "e1"),
            WalCursor::default(),
        )
        .unwrap();
    fixture
        .apply(
            &SimEvent::new(MutationKind::Insert, "A", 5, "inv-2", "e2"),
            WalCursor {
                seq: 1,
                ..WalCursor::default()
            },
        )
        .unwrap();
    assert_eq!(
        fixture
            .measure("customer_invoice_totals", "A", "invoice_total")
            .unwrap(),
        15.0
    );
    assert_eq!(
        fixture
            .measure("customer_invoice_totals", "A", "invoice_count")
            .unwrap(),
        2.0
    );
}

#[test]
fn g_keyed_update() {
    let fixture = DiskFixture::new();
    fixture
        .apply(
            &SimEvent::new(MutationKind::Insert, "A", 10, "inv", "e1"),
            WalCursor::default(),
        )
        .unwrap();
    fixture
        .apply(
            &SimEvent::new(MutationKind::Update, "A", 7, "inv", "e2")
                .with_before_row(SimEvent::new(MutationKind::Insert, "A", 10, "inv", "e1").row()),
            WalCursor {
                seq: 1,
                ..WalCursor::default()
            },
        )
        .unwrap();
    assert_eq!(
        fixture
            .measure("customer_invoice_totals", "A", "invoice_total")
            .unwrap(),
        7.0
    );
}

#[test]
fn g_keyed_delete() {
    let fixture = DiskFixture::new();
    fixture
        .apply(
            &SimEvent::new(MutationKind::Insert, "A", 10, "inv", "e1"),
            WalCursor::default(),
        )
        .unwrap();
    fixture
        .apply(
            &SimEvent::new(MutationKind::Delete, "A", 10, "inv", "e2"),
            WalCursor {
                seq: 1,
                ..WalCursor::default()
            },
        )
        .unwrap();
    assert_eq!(
        fixture
            .measure("customer_invoice_totals", "A", "invoice_total")
            .unwrap(),
        0.0
    );
}

#[test]
fn g_update_without_before_is_skipped() {
    let fixture = DiskFixture::new();
    fixture
        .apply(
            &SimEvent::new(MutationKind::Insert, "A", 10, "inv", "e0"),
            WalCursor::default(),
        )
        .unwrap();
    fixture
        .apply(
            &SimEvent::new(MutationKind::Update, "B", 10, "inv", "e1"),
            WalCursor {
                seq: 1,
                ..WalCursor::default()
            },
        )
        .unwrap();
    assert_eq!(
        fixture
            .measure("customer_invoice_totals", "A", "invoice_total")
            .unwrap(),
        10.0
    );
    assert_eq!(
        fixture
            .measure("customer_invoice_totals", "B", "invoice_total")
            .unwrap(),
        0.0
    );
}

#[test]
fn g_keyed_pk_change() {
    let fixture = DiskFixture::new();
    fixture
        .apply(
            &SimEvent::new(MutationKind::Insert, "A", 10, "inv", "e1"),
            WalCursor::default(),
        )
        .unwrap();
    fixture
        .apply(
            &SimEvent::new(MutationKind::Update, "B", 10, "inv", "e2")
                .with_before_row(SimEvent::new(MutationKind::Insert, "A", 10, "inv", "e1").row()),
            WalCursor {
                seq: 1,
                ..WalCursor::default()
            },
        )
        .unwrap();
    assert_eq!(
        fixture
            .measure("customer_invoice_totals", "A", "invoice_total")
            .unwrap(),
        0.0
    );
    assert_eq!(
        fixture
            .measure("customer_invoice_totals", "B", "invoice_total")
            .unwrap(),
        10.0
    );
}

#[test]
fn g_already_applied_slice_skip() {
    let fixture = DiskFixture::new();
    let batch = invoice_batch(&["A"], &[9], &["inv"]);
    let skipped = fixture
        .apply_events(
            &fixture.spec,
            WalCursor::default(),
            &batch,
            Some(&[insert_meta("e1")]),
            batch.num_rows(),
        )
        .unwrap();
    assert_eq!(skipped, WalCursor::default());
    assert_eq!(
        fixture
            .measure("customer_invoice_totals", "A", "invoice_total")
            .unwrap(),
        0.0
    );
}

#[test]
fn g_stateless_filter_derive() {
    use crate::compute::{apply_stateless, StatelessSelect};
    let batch = invoice_batch(&["a", "b"], &[0, 5], &["i1", "i2"]);
    let select = StatelessSelect {
        filter: Some("amount > 0".into()),
        derive: vec![("double".into(), "amount * 2".into())],
    };
    let aligned = apply_stateless(&[], Some(&select), batch, None).unwrap();
    assert_eq!(aligned.batch.num_rows(), 1);
    assert!(aligned.batch.schema().field_with_name("double").is_ok());
}

#[test]
fn g_stateless_unknown_filter() {
    let select = crate::compute::StatelessSelect {
        filter: Some("missing > 0".into()),
        derive: Vec::new(),
    };
    let batch = invoice_batch(&["a"], &[1], &["i1"]);
    assert!(crate::compute::apply_stateless(&[], Some(&select), batch, None).is_err());
}

#[test]
fn g_stateless_derived_column() {
    use crate::compute::{apply_stateless, StatelessSelect};
    let batch = invoice_batch(&["a"], &[3], &["i1"]);
    let select = StatelessSelect {
        filter: None,
        derive: vec![("amount2".into(), "amount + 1".into())],
    };
    let aligned = apply_stateless(&[], Some(&select), batch, None).unwrap();
    assert!(aligned.batch.schema().field_with_name("amount2").is_ok());
    assert!(aligned.batch.schema().field_with_name("amount").is_ok());
}

#[test]
fn g_create_compute_journals() {
    use super::{apply, Op, World};
    use crate::compute::{bind_statement, desugar_create, ir_hash, BindCatalog, FieldId, Fqn};
    let unbound = desugar_create(
        Fqn::from_parts("invoices", "customer_totals").unwrap(),
        "SELECT customer_id, SUM(amount) AS invoice_total FROM invoices.orders GROUP BY customer_id",
        false,
    )
    .unwrap();
    let mut ingest = std::collections::BTreeMap::new();
    ingest.insert(
        ("invoices".into(), "orders".into()),
        std::collections::BTreeMap::from([
            ("customer_id".into(), FieldId::new(1).unwrap()),
            ("amount".into(), FieldId::new(2).unwrap()),
        ]),
    );
    let catalog = BindCatalog {
        ingest,
        ingest_namespaces: ["orders".into()].into_iter().collect(),
        existing: Vec::new(),
        event_time: None,
        watermark_delay: None,
    };
    let kind = bind_statement(&catalog, unbound).unwrap();
    let mut world = World::new();
    apply(&mut world, Op::CreateCompute(kind.clone()));
    let key = crate::compute::journal_key("dst", "dst").unwrap();
    let log = crate::compute::load_journal(&world.catalog, &key).unwrap();
    assert_eq!(log.kinds(), vec![kind.clone()]);
    assert_eq!(
        log.get(kind.name()).unwrap().hash,
        crate::compute::ir_hash_kind(&kind).unwrap()
    );
}

#[test]
fn g_keyed_insert() {
    let fixture = DiskFixture::new();
    fixture
        .apply(
            &SimEvent::new(MutationKind::Insert, "A", 40, "inv", "e1"),
            WalCursor::default(),
        )
        .unwrap();
    assert_eq!(
        fixture
            .measure("customer_invoice_totals", "A", "invoice_total")
            .unwrap(),
        40.0
    );
}

#[test]
fn g_keyed_snapshot() {
    let fixture = DiskFixture::new();
    fixture
        .apply(
            &SimEvent::new(MutationKind::Insert, "A", 10, "inv", "e1"),
            WalCursor::default(),
        )
        .unwrap();
    fixture
        .apply(
            &SimEvent::new(MutationKind::Snapshot, "A", 4, "inv", "e2"),
            WalCursor {
                seq: 1,
                ..WalCursor::default()
            },
        )
        .unwrap();
    assert_eq!(
        fixture
            .measure("customer_invoice_totals", "A", "invoice_total")
            .unwrap(),
        14.0
    );
}

#[test]
fn g_window_retract_on_time_change() {
    use crate::processor::test_support::{hourly_kind, window_batch, window_store_key};
    let fixture = DiskFixture::with_spec(hourly_kind());
    let t12 = "2026-11-15T12:10:00Z";
    let t13 = "2026-11-15T13:10:00Z";
    fixture
        .apply_events(
            &fixture.spec,
            WalCursor::default(),
            &window_batch(&["A"], &[10], &["inv"], &[t12]),
            Some(&[insert_meta("e1")]),
            0,
        )
        .unwrap();
    fixture
        .apply_events(
            &fixture.spec,
            WalCursor {
                seq: 1,
                ..WalCursor::default()
            },
            &window_batch(&["A"], &[10], &["inv"], &[t13]),
            Some(&[
                crate::processor::test_support::meta(MutationKind::Update, "e2").with_before(
                    crate::plugins::cdc::BeforeImage {
                        key: b"A".to_vec(),
                        row: serde_json::to_vec(&BTreeMap::from([
                            (
                                "customer_id".to_string(),
                                serde_json::Value::String("A".into()),
                            ),
                            ("amount".to_string(), serde_json::Value::from(10)),
                            (
                                "invoice_id".to_string(),
                                serde_json::Value::String("inv".into()),
                            ),
                            (
                                "created_at".to_string(),
                                serde_json::Value::String(t12.into()),
                            ),
                        ]))
                        .unwrap(),
                    },
                ),
            ]),
            0,
        )
        .unwrap();
    assert_eq!(
        fixture
            .measure(
                "hourly_totals",
                &window_store_key("A", t12),
                "invoice_total"
            )
            .unwrap(),
        0.0
    );
    assert_eq!(
        fixture
            .measure(
                "hourly_totals",
                &window_store_key("A", t13),
                "invoice_total"
            )
            .unwrap(),
        10.0
    );
}

#[test]
fn g_window_late_after_compact() {
    use crate::processor::test_support::{hourly_kind, window_batch, window_store_key};
    let fixture = DiskFixture::with_spec(hourly_kind());
    let t13 = "2026-11-15T13:10:00Z";
    let t12 = "2026-11-15T12:10:00Z";
    fixture
        .apply_events(
            &fixture.spec,
            WalCursor::default(),
            &window_batch(&["A"], &[5], &["inv-1"], &[t13]),
            Some(&[insert_meta("e1")]),
            0,
        )
        .unwrap();
    fixture
        .apply_events(
            &fixture.spec,
            WalCursor {
                seq: 1,
                ..WalCursor::default()
            },
            &window_batch(&["A"], &[7], &["inv-2"], &[t12]),
            Some(&[insert_meta("e2")]),
            0,
        )
        .unwrap();
    assert_eq!(
        fixture
            .measure(
                "hourly_totals",
                &window_store_key("A", t13),
                "invoice_total"
            )
            .unwrap(),
        5.0
    );
    assert_eq!(
        fixture
            .measure(
                "hourly_totals",
                &window_store_key("A", t12),
                "invoice_total"
            )
            .unwrap(),
        7.0
    );
}
