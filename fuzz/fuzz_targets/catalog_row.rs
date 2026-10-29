#![no_main]

use libfuzzer_sys::fuzz_target;
use skippr_wal_catalog::{CatalogRow, CatalogTxn, PutCond};

fuzz_target!(|data: &[u8]| {
    let mut txn = CatalogTxn::new();
    if let Ok(row) = bincode::deserialize::<CatalogRow>(data) {
        let _ = txn.put(
            skippr_wal_catalog::CatalogKey::Format,
            row,
            PutCond::NotExists,
        );
    }
    if let Ok(row) = serde_json::from_slice::<CatalogRow>(data) {
        let _ = txn.put(
            skippr_wal_catalog::CatalogKey::Format,
            row,
            PutCond::NotExists,
        );
    }
});
