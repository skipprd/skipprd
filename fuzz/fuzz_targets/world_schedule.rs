#![no_main]

use libfuzzer_sys::fuzz_target;
use skipprd::sqlrt::dst::{current_table, run, schedule_from_bytes};

fuzz_target!(|data: &[u8]| {
    let world = run(&schedule_from_bytes(data));
    let _ = current_table(&world);
});
