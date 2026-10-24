//! Crash injection on local-FS Sled: every durable boundary, then reopen + replay.

use super::test_support::*;
use super::*;
use crate::plugins::cdc::MutationKind;

fn chaos_scripts() -> Vec<Vec<SimEvent>> {
    vec![
        vec![SimEvent::new(MutationKind::Insert, "A", 7, "inv", "e1")],
        vec![
            SimEvent::new(MutationKind::Insert, "A", 100, "inv", "e1"),
            SimEvent::new(MutationKind::Update, "A", 150, "inv", "e2"),
        ],
        vec![
            SimEvent::new(MutationKind::Insert, "A", 25, "inv", "e1"),
            SimEvent::new(MutationKind::Update, "B", 25, "inv", "e2"),
        ],
        vec![
            SimEvent::new(MutationKind::Insert, "A", 80, "inv", "e1"),
            SimEvent::new(MutationKind::Delete, "A", 80, "inv", "e2"),
        ],
        vec![
            SimEvent::new(MutationKind::Insert, "A", 1, "i1", "a"),
            SimEvent::new(MutationKind::Insert, "B", 2, "i2", "b"),
        ],
        vec![
            SimEvent::new(MutationKind::Update, "A", 40, "inv", "missing"),
            SimEvent::new(MutationKind::Insert, "A", 5, "inv-2", "e2"),
        ],
    ]
}

fn run_script_with_crash(script: &[SimEvent], crash_at: usize, point: CrashPoint) {
    let mut fixture = DiskFixture::new();
    let mut model = Model::default();
    let mut cursor = WalCursor::default();
    for event in &script[..crash_at] {
        model.apply(&fixture.spec, event);
        cursor = fixture.apply(event, cursor).unwrap();
        fixture = fixture.reopen();
    }
    let before = fixture.snapshot_kv();
    let event = &script[crash_at];
    let (fixture, panicked) = fixture.crash_one(event, cursor.clone(), point);
    assert!(panicked, "crash point {point:?} must unwind");
    if commit_survives(point) {
        model.apply(&fixture.spec, event);
        fixture.assert_matches(&model);
        let after = fixture.snapshot_kv();
        assert_ne!(
            before, after,
            "AfterTxn must persist the atomic commit on disk"
        );
    } else {
        fixture.assert_matches(&model);
        let after = fixture.snapshot_kv();
        assert_eq!(
            before, after,
            "{point:?} must abort the whole txn: measures, last-row, entity, dedup, cursor"
        );
    }
    let mut recovered = cursor.clone();
    if commit_survives(point) {
        recovered.row_index = recovered.row_index.saturating_add(1);
    }
    for event in &script[crash_at..] {
        recovered = fixture.apply(event, recovered).unwrap();
        model.apply(&fixture.spec, event);
    }
    let fixture = fixture.reopen();
    fixture.assert_matches(&model);
}

#[test]
fn crash_matrix_every_boundary_then_replay_matches_model() {
    for script in chaos_scripts() {
        for crash_at in 0..script.len() {
            for &point in crash_points() {
                run_script_with_crash(&script, crash_at, point);
            }
        }
    }
}

#[test]
fn multi_row_batch_is_atomic_across_crash() {
    let spec = invoice_agg();
    let batch = invoice_batch(&["A", "B", "C"], &[10, 20, 30], &["i1", "i2", "i3"]);
    let cdc = vec![insert_meta("e1"), insert_meta("e2"), insert_meta("e3")];
    for &point in crash_points() {
        let fixture = DiskFixture::new();
        let before = fixture.snapshot_kv();
        set_test_crash(Some(point));
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = fixture.apply_events(&spec, WalCursor::default(), &batch, Some(&cdc), 0);
        }))
        .is_err();
        set_test_crash(None);
        assert!(panicked);
        let fixture = fixture.reopen();
        if commit_survives(point) {
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
                20.0
            );
            assert_eq!(
                fixture
                    .measure("customer_invoice_totals", "C", "invoice_total")
                    .unwrap(),
                30.0
            );
        } else {
            assert_eq!(fixture.snapshot_kv(), before);
            assert_eq!(
                fixture
                    .measure("customer_invoice_totals", "A", "invoice_total")
                    .unwrap(),
                0.0
            );
            assert_eq!(
                fixture
                    .measure("customer_invoice_totals", "C", "invoice_total")
                    .unwrap(),
                0.0
            );
        }
        fixture
            .apply_events(&spec, WalCursor::default(), &batch, Some(&cdc), 0)
            .unwrap();
        let fixture = fixture.reopen();
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
            20.0
        );
        assert_eq!(
            fixture
                .measure("customer_invoice_totals", "C", "invoice_count")
                .unwrap(),
            1.0
        );
    }
}

#[test]
fn after_txn_crash_restores_reclaim_hold_from_disk_cursor() {
    reset_reclaim_hold();
    let event = SimEvent::new(MutationKind::Insert, "A", 3, "inv", "e1");
    let fixture = DiskFixture::new();
    let cursor = WalCursor {
        seq: 9,
        segment_id: "seg-9".into(),
        ..WalCursor::default()
    };
    set_min_unconsumed_seq(u64::MAX);
    let (fixture, panicked) = fixture.crash_one(&event, cursor, CrashPoint::AfterTxn);
    assert!(panicked);
    assert_eq!(
        fixture
            .measure("customer_invoice_totals", "A", "invoice_total")
            .unwrap(),
        3.0
    );
    assert!(
        !allows_reclaim(9),
        "reopen must restore reclaim hold from cursor"
    );
    reset_reclaim_hold();
}
