#![no_main]

use iceberg::spec::TableMetadata;
use libfuzzer_sys::fuzz_target;
use skipprd::wal_slice_ref::decode_wal_slices;

fuzz_target!(|data: &[u8]| {
    if let Ok(meta) = serde_json::from_slice::<TableMetadata>(data) {
        let _ = meta.current_schema();
        if let Some(raw) = meta.properties().get("skippr.wal-slices") {
            let _ = decode_wal_slices(raw);
        }
        for snapshot in meta.snapshots() {
            if let Some(raw) = snapshot
                .summary()
                .additional_properties
                .get("skippr.wal-slices")
            {
                let _ = decode_wal_slices(raw);
            }
        }
        return;
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(data) else {
        return;
    };
    if let Some(raw) = value
        .get("properties")
        .and_then(|props| props.get("skippr.wal-slices"))
        .and_then(|v| v.as_str())
    {
        let _ = decode_wal_slices(raw);
    }
});
