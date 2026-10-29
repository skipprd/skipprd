#![no_main]

use libfuzzer_sys::fuzz_target;
use skipprd::plugins::cdc::MutationKind;
use skipprd::sqlrt::cdc_reconcile::{attach_cdc_sidecar, collapse_wal_batches};
use skipprd::sqlrt::dst::{batch_from_row, UserRow};

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    let keys = vec!["id".to_string()];
    let mut batches = Vec::new();
    for (i, b) in data.iter().take(8).enumerate() {
        let row = UserRow {
            id: (*b as i64 % 4) + 1,
            name: "k".into(),
            amount: *b as i64,
        };
        let mutation = match b % 4 {
            0 => MutationKind::Snapshot,
            1 => MutationKind::Insert,
            2 => MutationKind::Update,
            _ => MutationKind::Delete,
        };
        if let Ok(batch) = attach_cdc_sidecar(
            batch_from_row(&row),
            &[mutation],
            &[vec![*b]],
            i as u64 + 1,
            0,
            0,
        ) {
            batches.push(batch);
        }
    }
    if !batches.is_empty() {
        let _ = collapse_wal_batches(&batches, &keys);
    }
});
