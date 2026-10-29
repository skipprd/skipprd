#![no_main]

use libfuzzer_sys::fuzz_target;
use skipprd::helpers::Helpers;
use std::collections::HashMap;

fuzz_target!(|data: &[u8]| {
    if data.len() > 4096 {
        return;
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(data) else {
        return;
    };
    let _ = Helpers::flatten(&value, &HashMap::new());
});
