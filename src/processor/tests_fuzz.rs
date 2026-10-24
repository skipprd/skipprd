//! Property-based fuzz of CDC streams, crashes, and disk recovery.

use super::test_support::*;
use super::*;
use crate::plugins::cdc::MutationKind;
use proptest::prelude::*;

fn arb_mutation() -> impl Strategy<Value = MutationKind> {
    prop_oneof![
        Just(MutationKind::Insert),
        Just(MutationKind::Snapshot),
        Just(MutationKind::Update),
        Just(MutationKind::Delete),
    ]
}

fn arb_script() -> impl Strategy<Value = Vec<SimEvent>> {
    (1usize..12).prop_flat_map(|n| {
        proptest::collection::vec(
            (
                arb_mutation(),
                prop_oneof![Just("A"), Just("B"), Just("C")],
                1i64..40,
                prop_oneof![Just("inv-1"), Just("inv-2"), Just("inv-3")],
            ),
            n,
        )
        .prop_map(|rows| {
            rows.into_iter()
                .enumerate()
                .map(|(i, (mutation, customer, amount, invoice))| {
                    SimEvent::new(mutation, customer, amount, invoice, format!("e{i}"))
                })
                .collect()
        })
    })
}

fn arb_crash_point() -> impl Strategy<Value = CrashPoint> {
    prop_oneof![
        Just(CrashPoint::BeforeTxn),
        Just(CrashPoint::BeforeCommit),
        Just(CrashPoint::DuringTxn),
        Just(CrashPoint::AfterTxn),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(stream_test_cases()))]

    #[test]
    fn fuzz_disk_replay_matches_model(script in arb_script()) {
        let fixture = DiskFixture::new();
        let mut model = Model::default();
        for event in &script {
            model.apply(&fixture.spec, event);
        }
        fixture.apply_all(&script);
        let fixture = fixture.reopen();
        fixture.assert_matches(&model);
    }

    #[test]
    fn fuzz_crash_then_reopen_then_replay(
        script in arb_script(),
        point in arb_crash_point(),
        crash_slot in 0usize..12,
    ) {
        prop_assume!(!script.is_empty());
        let crash_at = crash_slot % script.len();
        let fixture = DiskFixture::new();
        let mut model = Model::default();
        let mut cursor = WalCursor::default();
        for event in &script[..crash_at] {
            model.apply(&fixture.spec, event);
            cursor = fixture.apply(event, cursor).unwrap();
        }
        let before = fixture.snapshot_kv();
        let (fixture, panicked) = fixture.crash_one(&script[crash_at], cursor.clone(), point);
        prop_assert!(panicked);
        if commit_survives(point) {
            model.apply(&fixture.spec, &script[crash_at]);
        } else {
            prop_assert_eq!(fixture.snapshot_kv(), before);
        }
        fixture.assert_matches(&model);
        let mut recovered = cursor;
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
}
