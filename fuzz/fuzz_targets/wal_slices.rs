#![no_main]

use libfuzzer_sys::fuzz_target;
use skipprd::buffer::segment_file::SegmentFile;
use skipprd::wal_slice_ref::decode_wal_slices;

fuzz_target!(|data: &[u8]| {
    if let Ok(raw) = std::str::from_utf8(data) {
        let _ = decode_wal_slices(raw);
    }
    let _ = SegmentFile::read_metadata_from_bytes(data);
});
