#![no_main]

use libfuzzer_sys::fuzz_target;
use skipprd::migration::{load_sql_file, MigrationChecksum};

fuzz_target!(|data: &[u8]| {
    if data.len() > 4096 {
        return;
    }
    if let Ok(sql) = std::str::from_utf8(data) {
        let _ = MigrationChecksum::of(sql);
        let _ = load_sql_file("fuzz", sql);
    }
    let _ = bincode::deserialize::<MigrationChecksum>(data);
});
