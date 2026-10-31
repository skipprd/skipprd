#![no_main]

use libfuzzer_sys::fuzz_target;
use skipprd::failpoint::FailpointName;

fuzz_target!(|data: &[u8]| {
    if let Ok(raw) = std::str::from_utf8(data) {
        let _ = FailpointName::parse(raw);
    }
});
