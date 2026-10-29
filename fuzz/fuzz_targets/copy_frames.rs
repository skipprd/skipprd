#![no_main]

use libfuzzer_sys::fuzz_target;
use skipprd::runtime_plugins::protocol::{HostFrame, PluginFrame};
use skipprd::runtime_plugins::wire::decode_payload;

fuzz_target!(|data: &[u8]| {
    let _ = decode_payload::<HostFrame>(data);
    let _ = decode_payload::<PluginFrame>(data);
});
