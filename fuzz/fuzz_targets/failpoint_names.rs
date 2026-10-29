#![no_main]

use libfuzzer_sys::fuzz_target;
use skipprd::cluster::failpoint::FailpointName;

fuzz_target!(|data: &[u8]| {
    if let Ok(raw) = std::str::from_utf8(data) {
        let _ = FailpointName::parse(raw);
    }
});
