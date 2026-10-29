#![no_main]

use libfuzzer_sys::fuzz_target;
use skipprd::plugins::cdc::CheckpointEnvelope;

fuzz_target!(|data: &[u8]| {
    let _ = bincode::deserialize::<CheckpointEnvelope>(data);
});
