#![no_main]

use libfuzzer_sys::fuzz_target;
use skipprd::helpers::configuration::Config;

fuzz_target!(|data: &[u8]| {
    if data.len() > 4096 {
        return;
    }
    let Ok(raw) = std::str::from_utf8(data) else {
        return;
    };
    let _ = serde_yaml::from_str::<Config>(raw);
});
