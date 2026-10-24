//! Deterministic simulation: scripted CDC streams vs a golden in-memory model.

use super::test_support::*;
use super::*;
use crate::plugins::cdc::MutationKind;

fn scripts() -> Vec<Vec<SimEvent>> {
    vec![
        vec![SimEvent::new(MutationKind::Insert, "A", 100, "inv-1", "e1")],
        vec![
            SimEvent::new(MutationKind::Insert, "A", 100, "inv-1", "e1"),
            SimEvent::new(MutationKind::Insert, "A", 50, "inv-2", "e2"),
            SimEvent::new(MutationKind::Insert, "B", 10, "inv-3", "e3"),
        ],
        vec![
            SimEvent::new(MutationKind::Insert, "A", 100, "inv", "e1"),
            SimEvent::new(MutationKind::Update, "A", 150, "inv", "e2"),
        ],
        vec![
            SimEvent::new(MutationKind::Insert, "A", 80, "inv", "e1"),
            SimEvent::new(MutationKind::Delete, "A", 80, "inv", "e2"),
        ],
        vec![
            SimEvent::new(MutationKind::Insert, "A", 25, "inv", "e1"),
            SimEvent::new(MutationKind::Update, "B", 25, "inv", "e2"),
        ],
        vec![SimEvent::new(MutationKind::Update, "A", 40, "inv", "e1")],
        vec![SimEvent::new(MutationKind::Delete, "A", 9, "inv", "e1")],
        vec![
            SimEvent::new(MutationKind::Insert, "A", 40, "inv", "same"),
            SimEvent::new(MutationKind::Insert, "A", 40, "inv", "same"),
        ],
        vec![
            SimEvent::new(MutationKind::Snapshot, "A", 5, "s1", "s1"),
            SimEvent::new(MutationKind::Insert, "B", 7, "s2", "s2"),
            SimEvent::new(MutationKind::Update, "B", 11, "s2", "s3"),
            SimEvent::new(MutationKind::Delete, "A", 5, "s1", "s4"),
            SimEvent::new(MutationKind::Update, "C", 3, "s2", "s5"),
        ],
        vec![
            SimEvent::new(MutationKind::Insert, "A", 1, "i1", "a"),
            SimEvent::new(MutationKind::Insert, "B", 2, "i2", "b"),
            SimEvent::new(MutationKind::Insert, "C", 3, "i3", "c"),
            SimEvent::new(MutationKind::Update, "A", 4, "i1", "d"),
            SimEvent::new(MutationKind::Delete, "B", 2, "i2", "e"),
            SimEvent::new(MutationKind::Update, "D", 4, "i1", "f"),
        ],
    ]
}

#[test]
fn disk_sled_scripts_match_golden_model() {
    for (i, script) in scripts().into_iter().enumerate() {
        let fixture = DiskFixture::new();
        assert!(
            fixture.db_path.is_dir() || fixture.db_path.exists(),
            "script {i} must use a local sled path"
        );
        let mut model = Model::default();
        for event in &script {
            model.apply(&fixture.spec, event);
        }
        fixture.apply_all(&script);
        let fixture = fixture.reopen();
        fixture.assert_matches(&model);
        let meta = fixture
            .load_meta(&fixture.spec.id)
            .unwrap()
            .expect("cursor committed");
        assert!(
            meta.cursor.row_index >= 1,
            "script {i} must persist a WAL cursor on disk"
        );
    }
}

#[test]
fn serialized_commits_flush_each_event_to_disk() {
    let script = vec![
        SimEvent::new(MutationKind::Insert, "A", 100, "inv-1", "e1"),
        SimEvent::new(MutationKind::Insert, "A", 50, "inv-2", "e2"),
        SimEvent::new(MutationKind::Update, "A", 75, "inv-2", "e3"),
        SimEvent::new(MutationKind::Insert, "B", 9, "inv-3", "e4"),
    ];
    let mut fixture = DiskFixture::new();
    let mut model = Model::default();
    let mut cursor = WalCursor::default();
    for event in &script {
        model.apply(&fixture.spec, event);
        cursor = fixture.apply(event, cursor).unwrap();
        fixture = fixture.reopen();
        fixture.assert_matches(&model);
    }
}

#[test]
fn config_hash_rebuild_clears_disk_prefix() {
    let event = SimEvent::new(MutationKind::Insert, "A", 9, "inv", "e1");
    let fixture = DiskFixture::new();
    fixture.apply(&event, WalCursor::default()).unwrap();
    assert_eq!(
        fixture
            .measure("customer_invoice_totals", "A", "invoice_total")
            .unwrap(),
        9.0
    );
    let (dir, db_path) = fixture.release();
    let mut rebuilt = invoice_agg();
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
fn reclaim_hold_tracks_committed_cursor_after_reopen() {
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
