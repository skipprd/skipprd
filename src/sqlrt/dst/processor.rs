use crate::helpers::configuration::AggregateMeasure;
use crate::plugins::cdc::MutationKind;
use crate::processor::test_support::{insert_meta, invoice_batch, open_db, DiskFixture, SimEvent};
use crate::processor::{
    allows_reclaim, reset_reclaim_hold, set_min_unconsumed_seq, CrashPoint, WalCursor,
};

#[test]
fn c_reclaim_vs_stream_processor_cursor() {
    set_min_unconsumed_seq(5);
    assert!(!allows_reclaim(5), "must not delete seq still held");
    assert!(allows_reclaim(4));
    set_min_unconsumed_seq(u64::MAX);
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
fn g_config_hash_mismatch_rebuilds() {
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
    let (dir, db_path) = fixture.release();
    let mut rebuilt = spec;
    rebuilt.measures.insert(
        "other".into(),
        AggregateMeasure {
            count: Some("*".into()),
            ..Default::default()
        },
    );
    let (_db, state) = open_db(&db_path, rebuilt);
    assert_eq!(
        state
            .measure("customer_invoice_totals", "A", "invoice_total")
            .unwrap(),
        0.0
    );
    let _keep = dir;
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
    use crate::helpers::configuration::TransformOp;
    use crate::plugins::cdc::WalRowMeta;
    use crate::processor::test_support::invoice_batch;
    use crate::stream_transform::apply_configured_ops;
    let batch = invoice_batch(&["a", "b", "c"], &[0, 5, 0], &["i1", "i2", "i3"]);
    let cdc = vec![
        WalRowMeta {
            mutation: MutationKind::Insert,
            event_id: b"a".to_vec(),
            order_token: vec![1],
        },
        WalRowMeta {
            mutation: MutationKind::Insert,
            event_id: b"b".to_vec(),
            order_token: vec![2],
        },
        WalRowMeta {
            mutation: MutationKind::Insert,
            event_id: b"c".to_vec(),
            order_token: vec![3],
        },
    ];
    let ops = vec![TransformOp {
        filter: Some("amount > 0".into()),
        ..Default::default()
    }];
    let (batches, cdc) = apply_configured_ops(&ops, vec![batch], Some(cdc)).unwrap();
    assert_eq!(batches[0].num_rows(), 1);
    let cdc = cdc.unwrap();
    assert_eq!(cdc.len(), 1);
    assert_eq!(cdc[0].event_id, b"b");
}

#[test]
fn g_transform_error_does_not_advance_offsets() {
    use crate::helpers::offset_store::{apply_offset_field, OffsetStore, SledOffsetStore};
    use crate::helpers::offsets::OffsetTypes;
    use sha2::{Digest, Sha256};
    use skippr_lease::PublishMode;
    let ops = vec![crate::helpers::configuration::TransformOp {
        project: Some(vec!["missing".into()]),
        ..Default::default()
    }];
    let batch = invoice_batch(&["a"], &[1], &["i1"]);
    let dir = tempfile::tempdir().unwrap();
    let db = sled::open(dir.path()).unwrap();
    let store = SledOffsetStore::new(db.open_tree("offsets").unwrap());
    let sk = SledOffsetStore::offset_key("ns", "p");
    let before = apply_offset_field(None, OffsetTypes::Position, 9, false).unwrap();
    let sha = hex::encode(Sha256::digest(&before));
    store
        .publish_wal_commit(&sk, &before, 1, 1, &sha, PublishMode::Fresh)
        .unwrap();
    assert!(crate::stream_transform::apply_ops(&ops, vec![batch]).is_err());
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
    assert!(!allows_reclaim(4));
    assert!(allows_reclaim(3));
    let fixture = fixture.reopen();
    let meta = fixture
        .load_meta("customer_invoice_totals")
        .unwrap()
        .unwrap();
    assert_eq!(meta.cursor.seq, 4);
    assert!(!allows_reclaim(4));
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
fn g_cdc_update_delta() {
    let fixture = DiskFixture::new();
    fixture
        .apply(
            &SimEvent::new(MutationKind::Insert, "A", 10, "inv", "e1"),
            WalCursor::default(),
        )
        .unwrap();
    fixture
        .apply(
            &SimEvent::new(MutationKind::Update, "A", 7, "inv", "e2"),
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
fn g_delete_subtract() {
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
fn g_missing_last_row_as_insert() {
    let fixture = DiskFixture::new();
    fixture
        .apply(
            &SimEvent::new(MutationKind::Update, "A", 10, "inv", "e1"),
            WalCursor::default(),
        )
        .unwrap();
    assert_eq!(
        fixture
            .measure("customer_invoice_totals", "A", "invoice_total")
            .unwrap(),
        10.0
    );
}

#[test]
fn g_pk_change_pending_index() {
    let fixture = DiskFixture::new();
    fixture
        .apply(
            &SimEvent::new(MutationKind::Insert, "A", 10, "inv", "e1"),
            WalCursor::default(),
        )
        .unwrap();
    fixture
        .apply(
            &SimEvent::new(MutationKind::Update, "B", 10, "inv", "e2"),
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
fn g_filter_project_derive_compose() {
    use crate::helpers::configuration::TransformOp;
    use crate::stream_transform::apply_configured_ops;
    use std::collections::BTreeMap;
    let batch = invoice_batch(&["a", "b"], &[0, 5], &["i1", "i2"]);
    let ops = vec![
        TransformOp {
            filter: Some("amount > 0".into()),
            ..Default::default()
        },
        TransformOp {
            derive: Some(BTreeMap::from([("double".into(), "amount * 2".into())])),
            ..Default::default()
        },
        TransformOp {
            project: Some(vec!["customer_id".into(), "double".into()]),
            ..Default::default()
        },
    ];
    let (batches, _) = apply_configured_ops(&ops, vec![batch], None).unwrap();
    assert_eq!(batches[0].num_rows(), 1);
    assert!(batches[0].schema().field_with_name("double").is_ok());
    assert!(batches[0].schema().field_with_name("amount").is_err());
}

#[test]
fn g_unknown_project() {
    let ops = vec![crate::helpers::configuration::TransformOp {
        project: Some(vec!["missing".into()]),
        ..Default::default()
    }];
    let batch = invoice_batch(&["a"], &[1], &["i1"]);
    assert!(crate::stream_transform::apply_ops(&ops, vec![batch]).is_err());
}

#[test]
fn g_schema_change_derived_column() {
    use crate::helpers::configuration::TransformOp;
    use crate::stream_transform::apply_configured_ops;
    use std::collections::BTreeMap;
    let batch = invoice_batch(&["a"], &[3], &["i1"]);
    let ops = vec![TransformOp {
        derive: Some(BTreeMap::from([("amount2".into(), "amount + 1".into())])),
        ..Default::default()
    }];
    let (batches, _) = apply_configured_ops(&ops, vec![batch], None).unwrap();
    assert!(batches[0].schema().field_with_name("amount2").is_ok());
    assert!(batches[0].schema().field_with_name("amount").is_ok());
}
