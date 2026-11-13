#![no_main]

use iceberg::spec::{NestedField, PrimitiveType, Schema, Transform, Type};
use libfuzzer_sys::fuzz_target;
use skippr_iceberg_writer::layout_partition_spec;
use skippr_runtime_sdk::protocol::RuntimeOutputLayout;

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    let unit = match data[0] % 7 {
        0 => "year",
        1 => "month",
        2 => "day",
        3 => "hour",
        4 => "minute",
        5 => "week",
        _ => "",
    };
    let deadletter = data.get(2).copied().unwrap_or(0) % 3 == 0;
    let identity = if data.len() > 1 && data[1] % 2 == 0 {
        vec!["rmc".to_string()]
    } else {
        Vec::new()
    };
    let schema = Schema::builder()
        .with_schema_id(0)
        .with_fields(vec![
            NestedField::required(1, "rmc", Type::Primitive(PrimitiveType::String)).into(),
            NestedField::required(2, "time", Type::Primitive(PrimitiveType::Timestamp)).into(),
        ])
        .build()
        .unwrap();
    let layout = RuntimeOutputLayout {
        partition_fields: identity,
        time_partition_granularity: if unit.is_empty() {
            None
        } else {
            Some(unit.into())
        },
        batch_time_fields: vec!["time".into()],
        ..RuntimeOutputLayout::default()
    };
    match layout_partition_spec(&schema, &layout, deadletter) {
        Ok(None) => {}
        Ok(Some(spec)) => {
            for field in spec.fields() {
                if field.transform != Transform::Identity {
                    assert_eq!(
                        field.source_id, 2,
                        "time transforms must source the timestamp"
                    );
                }
            }
        }
        Err(_) => {}
    }
});
