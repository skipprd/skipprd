use std::io;
use std::sync::Arc;

use arrow::array::{
    Array, Date32Array, Date64Array, Decimal128Array, Float32Array, Float64Array, Int32Array,
    Int64Array, LargeStringArray, RecordBatch, StringArray, TimestampMicrosecondArray,
    TimestampMillisecondArray, TimestampNanosecondArray, TimestampSecondArray, UInt32Array,
    UInt64Array,
};
use arrow::datatypes::{DataType, Field, Schema as ArrowSchema, TimeUnit};
use arrow::util::display::array_value_to_string;
use iceberg::spec::{NestedField, PrimitiveType, Schema, Transform, Type, UnboundPartitionSpec};
use skippr_runtime_sdk::helpers::Helpers;
use skippr_runtime_sdk::protocol::RuntimeOutputLayout;

pub(crate) const SPEC_MISMATCH_RECREATE: &str =
    "Iceberg table partition spec does not match batch_time_unit / batch_partition_fields; recreate the table";

pub(crate) fn iceberg_time_transform(unit: &str) -> Result<Transform, io::Error> {
    match unit.trim().to_ascii_lowercase().as_str() {
        "year" => Ok(Transform::Year),
        "month" => Ok(Transform::Month),
        "day" => Ok(Transform::Day),
        "hour" => Ok(Transform::Hour),
        "minute" => Err(io::Error::other(
            "Iceberg has no minute() transform; unset batch_time_unit or use year, month, day, or hour",
        )),
        other if other.is_empty() => Err(io::Error::other(
            "Iceberg time partition unit is empty",
        )),
        other => Err(io::Error::other(format!(
            "unsupported Iceberg batch_time_unit '{other}'"
        ))),
    }
}

pub(crate) fn layout_has_iceberg_time_partition(layout: &RuntimeOutputLayout) -> bool {
    layout
        .time_partition_granularity
        .as_deref()
        .map(str::trim)
        .is_some_and(|unit| !unit.is_empty())
        && !layout.batch_time_fields.is_empty()
}

pub(crate) fn require_homogeneous_iceberg_partitions(
    layout: &RuntimeOutputLayout,
    deadletter: bool,
) -> bool {
    if deadletter {
        return false;
    }
    layout_has_iceberg_time_partition(layout)
        || layout
            .partition_fields
            .iter()
            .any(|field| !field.trim().is_empty())
}

/// Identity `batch_partition_fields` first, then one hidden time transform.
/// Unset unit and identity fields: OTLP `hour`/`service_name`/`tenant_id` when all three exist, else unpartitioned.
pub fn layout_partition_spec(
    schema: &Schema,
    layout: &RuntimeOutputLayout,
    deadletter: bool,
) -> Result<Option<UnboundPartitionSpec>, io::Error> {
    if deadletter {
        return Ok(None);
    }
    let mut builder = UnboundPartitionSpec::builder();
    let mut added = 0usize;

    for configured in &layout.partition_fields {
        let trimmed = configured.trim();
        if trimmed.is_empty() {
            continue;
        }
        let field = resolve_schema_field(schema, trimmed).ok_or_else(|| {
            io::Error::other(format!(
                "Iceberg identity partition field '{trimmed}' is not in the table schema"
            ))
        })?;
        builder = builder
            .add_partition_field(field.id, field.name.clone(), Transform::Identity)
            .map_err(|err| io::Error::other(err.to_string()))?;
        added += 1;
    }

    if layout_has_iceberg_time_partition(layout) {
        let unit = layout
            .time_partition_granularity
            .as_deref()
            .map(str::trim)
            .unwrap_or_default();
        let transform = iceberg_time_transform(unit)?;
        let source =
            first_present_time_field(schema, &layout.batch_time_fields).ok_or_else(|| {
                io::Error::other(format!(
                    "Iceberg time partition field [{}] is not in the table schema",
                    layout.batch_time_fields.join(", ")
                ))
            })?;
        fail_closed_time_source(&source.field_type, transform, &source.name)?;
        let part_name = format!("{}_{unit}", source.name);
        builder = builder
            .add_partition_field(source.id, part_name, transform)
            .map_err(|err| io::Error::other(err.to_string()))?;
        added += 1;
    }

    if added == 0 {
        return otlp_identity_spec(schema);
    }
    Ok(Some(builder.build()))
}

pub(crate) fn live_spec_matches_layout(
    schema: &Schema,
    live_fields: &[(i32, Transform)],
    layout: &RuntimeOutputLayout,
    deadletter: bool,
) -> Result<bool, io::Error> {
    let desired = layout_partition_spec(schema, layout, deadletter)?;
    let desired_fields: Vec<(i32, Transform)> = match desired {
        None => Vec::new(),
        Some(spec) => spec
            .fields()
            .iter()
            .map(|field| (field.source_id, field.transform))
            .collect(),
    };
    Ok(desired_fields == live_fields)
}

pub(crate) fn spec_mismatch_error() -> io::Error {
    io::Error::other(SPEC_MISMATCH_RECREATE)
}

fn otlp_identity_spec(schema: &Schema) -> Result<Option<UnboundPartitionSpec>, io::Error> {
    const NAMES: [&str; 3] = ["hour", "service_name", "tenant_id"];
    if !NAMES
        .iter()
        .all(|name| schema.field_by_name(name).is_some())
    {
        return Ok(None);
    }
    let mut builder = UnboundPartitionSpec::builder();
    for name in NAMES {
        let field = schema
            .field_by_name(name)
            .expect("OTLP field presence checked");
        builder = builder
            .add_partition_field(field.id, name, Transform::Identity)
            .map_err(|err| io::Error::other(err.to_string()))?;
    }
    Ok(Some(builder.build()))
}

fn fail_closed_time_source(
    field_type: &Type,
    transform: Transform,
    field_name: &str,
) -> Result<(), io::Error> {
    if transform == Transform::Hour && matches!(field_type, Type::Primitive(PrimitiveType::Date)) {
        return Err(io::Error::other(format!(
            "Iceberg hour() requires timestamp, not date, on '{field_name}'; recreate the table or pin schema"
        )));
    }
    transform.result_type(field_type).map_err(|_| {
        io::Error::other(format!(
            "{SPEC_MISMATCH_RECREATE} (time field '{field_name}' cannot take {transform})"
        ))
    })?;
    Ok(())
}

pub(crate) fn first_present_time_field<'a>(
    schema: &'a Schema,
    configured: &[String],
) -> Option<&'a NestedField> {
    configured
        .iter()
        .find_map(|name| resolve_schema_field(schema, name))
}

pub(crate) fn resolve_schema_field<'a>(
    schema: &'a Schema,
    configured: &str,
) -> Option<&'a NestedField> {
    field_name_candidates(configured)
        .into_iter()
        .find_map(|name| schema.field_by_name(&name).map(|field| field.as_ref()))
}

pub(crate) fn field_name_candidates(configured: &str) -> Vec<String> {
    let trimmed = configured.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let mut names = vec![trimmed.to_string()];
    let flattened = trimmed.replace('.', "_");
    if flattened != trimmed {
        names.push(flattened.clone());
    }
    let stripped = trimmed.trim_matches('_');
    if stripped != trimmed && !stripped.is_empty() {
        names.push(stripped.to_string());
        let stripped_flat = stripped.replace('.', "_");
        if stripped_flat != stripped {
            names.push(stripped_flat);
        }
    }
    names
}

pub(crate) fn numeric_time_fields_to_promote(
    metadata: &skippr_runtime_sdk::discover::OutputMetadata,
    layout: &RuntimeOutputLayout,
    deadletter: bool,
) -> std::collections::HashSet<String> {
    use skippr_runtime_sdk::discover::SkipprDataType;
    let mut names = std::collections::HashSet::new();
    if deadletter || !layout_has_iceberg_time_partition(layout) {
        return names;
    }
    for configured in &layout.batch_time_fields {
        for (_, child) in metadata.child_fields() {
            let child_name = child.out_field_name();
            if !field_name_candidates(configured)
                .iter()
                .any(|alias| alias == child_name)
            {
                continue;
            }
            match child.determined_type() {
                SkipprDataType::Long
                | SkipprDataType::Integer
                | SkipprDataType::Byte
                | SkipprDataType::Short
                | SkipprDataType::TimestampMilli => {
                    names.insert(child_name.to_string());
                }
                _ => {}
            }
        }
    }
    names
}

/// Coerce the event-time column to timestamp micros. Does not materialize calendar ints.
pub(crate) fn stamp_time_partition_columns(
    batch: RecordBatch,
    layout: &RuntimeOutputLayout,
    iceberg_schema: &Schema,
) -> Result<RecordBatch, io::Error> {
    if !layout_has_iceberg_time_partition(layout) {
        return Ok(batch);
    }
    let unit = layout
        .time_partition_granularity
        .as_deref()
        .map(str::trim)
        .unwrap_or_default();
    let transform = iceberg_time_transform(unit)?;
    let source =
        first_present_time_field(iceberg_schema, &layout.batch_time_fields).ok_or_else(|| {
            io::Error::other(format!(
                "{SPEC_MISMATCH_RECREATE} (missing time field [{}])",
                layout.batch_time_fields.join(", ")
            ))
        })?;
    fail_closed_time_source(&source.field_type, transform, &source.name)?;
    if batch.num_rows() == 0 {
        return coerce_event_time_to_timestamp(batch, &layout.batch_time_fields);
    }
    for row in 0..batch.num_rows() {
        if event_time_posix_seconds(&batch, &layout.batch_time_fields, row).is_none() {
            return Err(io::Error::other(format!(
                "missing or unresolvable event time in configured batch_time_fields [{}]",
                layout.batch_time_fields.join(", ")
            )));
        }
    }
    coerce_event_time_to_timestamp(batch, &layout.batch_time_fields)
}

fn coerce_event_time_to_timestamp(
    batch: RecordBatch,
    fields: &[String],
) -> Result<RecordBatch, io::Error> {
    let schema = batch.schema();
    let Some(idx) = fields
        .iter()
        .find_map(|field| column_index_for_time_field(schema.as_ref(), field))
    else {
        return Ok(batch);
    };
    let field = schema.field(idx);
    match field.data_type() {
        DataType::Timestamp(_, _) | DataType::Date32 | DataType::Date64 => return Ok(batch),
        _ => {}
    }
    let mut micros: Vec<Option<i64>> = Vec::with_capacity(batch.num_rows());
    for row in 0..batch.num_rows() {
        micros.push(
            event_time_posix_seconds(&batch, fields, row)
                .map(|secs| secs.saturating_mul(1_000_000)),
        );
    }
    let ts = TimestampMicrosecondArray::from(micros);
    let mut fields_out: Vec<Arc<Field>> = schema.fields().iter().cloned().collect();
    fields_out[idx] = Arc::new(
        Field::new(
            field.name(),
            DataType::Timestamp(TimeUnit::Microsecond, None),
            field.is_nullable(),
        )
        .with_metadata(field.metadata().clone()),
    );
    let mut columns = batch.columns().to_vec();
    columns[idx] = Arc::new(ts);
    RecordBatch::try_new(Arc::new(ArrowSchema::new(fields_out)), columns)
        .map_err(|err| io::Error::other(err.to_string()))
}

pub(crate) fn event_time_posix_seconds(
    batch: &RecordBatch,
    fields: &[String],
    row: usize,
) -> Option<i64> {
    let schema = batch.schema();
    for field in fields {
        let Some(idx) = column_index_for_time_field(schema.as_ref(), field) else {
            continue;
        };
        if let Some(secs) = posix_seconds_from_arrow_cell(batch.column(idx), row) {
            return Some(secs);
        }
    }
    None
}

fn column_index_for_time_field(schema: &ArrowSchema, field: &str) -> Option<usize> {
    field_name_candidates(field)
        .into_iter()
        .find_map(|name| schema.index_of(&name).ok())
}

pub(crate) fn posix_seconds_from_numeric_i64(time: i64) -> Option<i64> {
    if let Some(secs) = Helpers::posix_seconds_from_i64(time) {
        return Some(secs);
    }
    let abs = time.unsigned_abs();
    let digits = if abs == 0 { 1 } else { abs.ilog10() + 1 };
    match digits {
        16 | 17 => Some(time / 1_000_000),
        18 | 19 => Some(time / 1_000_000_000),
        _ => None,
    }
}

fn posix_seconds_from_arrow_cell(column: &dyn Array, row: usize) -> Option<i64> {
    if row >= column.len() || column.is_null(row) {
        return None;
    }
    if let Some(arr) = column.as_any().downcast_ref::<Int64Array>() {
        return posix_seconds_from_numeric_i64(arr.value(row));
    }
    if let Some(arr) = column.as_any().downcast_ref::<Int32Array>() {
        return Helpers::posix_seconds_from_i64(i64::from(arr.value(row)));
    }
    if let Some(arr) = column.as_any().downcast_ref::<UInt32Array>() {
        return Helpers::posix_seconds_from_i64(i64::from(arr.value(row)));
    }
    if let Some(arr) = column.as_any().downcast_ref::<UInt64Array>() {
        let value = arr.value(row);
        if value > i64::MAX as u64 {
            return None;
        }
        return posix_seconds_from_numeric_i64(value as i64);
    }
    if let Some(arr) = column.as_any().downcast_ref::<Float64Array>() {
        let value = arr.value(row);
        if !value.is_finite() || value > i64::MAX as f64 || value < i64::MIN as f64 {
            return None;
        }
        return posix_seconds_from_numeric_i64(value as i64);
    }
    if let Some(arr) = column.as_any().downcast_ref::<Float32Array>() {
        let value = arr.value(row);
        if !value.is_finite()
            || (value as f64) > i64::MAX as f64
            || (value as f64) < i64::MIN as f64
        {
            return None;
        }
        return posix_seconds_from_numeric_i64(value as i64);
    }
    if let Some(arr) = column.as_any().downcast_ref::<Decimal128Array>() {
        let DataType::Decimal128(_, scale) = column.data_type() else {
            return None;
        };
        let raw = arr.value(row);
        let divisor = 10i128.pow((*scale).max(0) as u32);
        if divisor == 0 {
            return None;
        }
        let scaled = raw / divisor;
        if scaled > i64::MAX as i128 || scaled < i64::MIN as i128 {
            return None;
        }
        return posix_seconds_from_numeric_i64(scaled as i64);
    }
    if let Some(arr) = column.as_any().downcast_ref::<StringArray>() {
        return Helpers::posix_seconds_from_str(arr.value(row));
    }
    if let Some(arr) = column.as_any().downcast_ref::<LargeStringArray>() {
        return Helpers::posix_seconds_from_str(arr.value(row));
    }
    if let Some(arr) = column.as_any().downcast_ref::<TimestampSecondArray>() {
        return Some(arr.value(row));
    }
    if let Some(arr) = column.as_any().downcast_ref::<TimestampMillisecondArray>() {
        return Helpers::posix_seconds_from_i64(arr.value(row));
    }
    if let Some(arr) = column.as_any().downcast_ref::<TimestampMicrosecondArray>() {
        return Some(arr.value(row) / 1_000_000);
    }
    if let Some(arr) = column.as_any().downcast_ref::<TimestampNanosecondArray>() {
        return Some(arr.value(row) / 1_000_000_000);
    }
    if let Some(arr) = column.as_any().downcast_ref::<Date32Array>() {
        return Some(i64::from(arr.value(row)) * 86_400);
    }
    if let Some(arr) = column.as_any().downcast_ref::<Date64Array>() {
        return Helpers::posix_seconds_from_i64(arr.value(row));
    }
    match column.data_type() {
        DataType::Timestamp(TimeUnit::Second, _) => column
            .as_any()
            .downcast_ref::<TimestampSecondArray>()
            .map(|arr| arr.value(row)),
        DataType::Timestamp(TimeUnit::Millisecond, _) => column
            .as_any()
            .downcast_ref::<TimestampMillisecondArray>()
            .and_then(|arr| Helpers::posix_seconds_from_i64(arr.value(row))),
        DataType::Timestamp(TimeUnit::Microsecond, _) => column
            .as_any()
            .downcast_ref::<TimestampMicrosecondArray>()
            .map(|arr| arr.value(row) / 1_000_000),
        DataType::Timestamp(TimeUnit::Nanosecond, _) => column
            .as_any()
            .downcast_ref::<TimestampNanosecondArray>()
            .map(|arr| arr.value(row) / 1_000_000_000),
        _ => {
            let displayed = array_value_to_string(column, row).ok()?;
            Helpers::posix_seconds_from_str(&displayed).or_else(|| {
                displayed
                    .parse::<i64>()
                    .ok()
                    .and_then(posix_seconds_from_numeric_i64)
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use arrow::array::{ArrayRef, Decimal128Array, Int64Array, TimestampMillisecondArray};
    use iceberg::spec::{NestedField, PrimitiveType, Type};

    fn ts_schema(name: &str) -> Schema {
        Schema::builder()
            .with_schema_id(0)
            .with_fields(vec![NestedField::required(
                1,
                name,
                Type::Primitive(PrimitiveType::Timestamp),
            )
            .into()])
            .build()
            .unwrap()
    }

    fn day_layout(fields: &[&str]) -> RuntimeOutputLayout {
        RuntimeOutputLayout {
            time_partition_granularity: Some("day".into()),
            batch_time_fields: fields.iter().map(|field| (*field).to_string()).collect(),
            ..RuntimeOutputLayout::default()
        }
    }

    fn layout_unit(unit: &str, fields: &[&str]) -> RuntimeOutputLayout {
        RuntimeOutputLayout {
            time_partition_granularity: Some(unit.into()),
            batch_time_fields: fields.iter().map(|field| (*field).to_string()).collect(),
            ..RuntimeOutputLayout::default()
        }
    }

    #[test]
    fn layout_without_unit_or_fields_is_disabled() {
        assert!(!layout_has_iceberg_time_partition(
            &RuntimeOutputLayout::default()
        ));
        let unit_only = RuntimeOutputLayout {
            time_partition_granularity: Some("day".into()),
            ..RuntimeOutputLayout::default()
        };
        assert!(!layout_has_iceberg_time_partition(&unit_only));
    }

    #[test]
    fn unit_maps_one_to_one_to_iceberg_transform() {
        assert_eq!(iceberg_time_transform("year").unwrap(), Transform::Year);
        assert_eq!(iceberg_time_transform("month").unwrap(), Transform::Month);
        assert_eq!(iceberg_time_transform("DAY").unwrap(), Transform::Day);
        assert_eq!(iceberg_time_transform("hour").unwrap(), Transform::Hour);
        let err = iceberg_time_transform("minute").unwrap_err();
        assert!(err.to_string().contains("no minute()"), "{err}");
    }

    #[test]
    fn day_spec_is_day_transform_without_calendar_columns() {
        let schema = Schema::builder()
            .with_schema_id(0)
            .with_fields(vec![
                NestedField::required(1, "id", Type::Primitive(PrimitiveType::Long)).into(),
                NestedField::required(2, "event_ts", Type::Primitive(PrimitiveType::Timestamp))
                    .into(),
            ])
            .build()
            .unwrap();
        let spec = layout_partition_spec(&schema, &day_layout(&["event_ts"]), false)
            .unwrap()
            .unwrap();
        assert_eq!(spec.fields().len(), 1);
        assert_eq!(spec.fields()[0].source_id, 2);
        assert_eq!(spec.fields()[0].transform, Transform::Day);
        assert_eq!(spec.fields()[0].name, "event_ts_day");
        assert!(schema.field_by_name("year").is_none());
        assert!(schema.field_by_name("month").is_none());
        assert!(schema.field_by_name("day").is_none());
    }

    #[test]
    fn hour_month_year_specs() {
        let schema = ts_schema("time");
        for (unit, transform) in [
            ("hour", Transform::Hour),
            ("month", Transform::Month),
            ("year", Transform::Year),
        ] {
            let spec = layout_partition_spec(&schema, &layout_unit(unit, &["time"]), false)
                .unwrap()
                .unwrap();
            assert_eq!(spec.fields().len(), 1, "{unit}");
            assert_eq!(spec.fields()[0].transform, transform, "{unit}");
            assert_eq!(spec.fields()[0].source_id, 1, "{unit}");
        }
    }

    #[test]
    fn identity_fields_then_day_transform() {
        let schema = Schema::builder()
            .with_schema_id(0)
            .with_fields(vec![
                NestedField::required(1, "detail_rmc", Type::Primitive(PrimitiveType::String))
                    .into(),
                NestedField::required(2, "time", Type::Primitive(PrimitiveType::Timestamp)).into(),
            ])
            .build()
            .unwrap();
        let layout = RuntimeOutputLayout {
            partition_fields: vec!["detail.rmc".into()],
            time_partition_granularity: Some("day".into()),
            batch_time_fields: vec!["time".into()],
            ..RuntimeOutputLayout::default()
        };
        let spec = layout_partition_spec(&schema, &layout, false)
            .unwrap()
            .unwrap();
        assert_eq!(spec.fields().len(), 2);
        assert_eq!(spec.fields()[0].source_id, 1);
        assert_eq!(spec.fields()[0].transform, Transform::Identity);
        assert_eq!(spec.fields()[1].source_id, 2);
        assert_eq!(spec.fields()[1].transform, Transform::Day);
    }

    #[test]
    fn minute_fail_closed_on_spec() {
        let err =
            layout_partition_spec(&ts_schema("time"), &layout_unit("minute", &["time"]), false)
                .unwrap_err();
        assert!(err.to_string().contains("no minute()"), "{err}");
    }

    #[test]
    fn hour_on_date_fail_closed() {
        let schema = Schema::builder()
            .with_schema_id(0)
            .with_fields(vec![NestedField::required(
                1,
                "time",
                Type::Primitive(PrimitiveType::Date),
            )
            .into()])
            .build()
            .unwrap();
        let err =
            layout_partition_spec(&schema, &layout_unit("hour", &["time"]), false).unwrap_err();
        assert!(
            err.to_string().contains("hour() requires timestamp"),
            "{err}"
        );
        assert!(err.to_string().contains("not date"), "{err}");
    }

    #[test]
    fn deadletter_is_unpartitioned() {
        assert!(
            layout_partition_spec(&ts_schema("time"), &day_layout(&["time"]), true)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn otlp_identity_when_layout_unset() {
        let schema = Schema::builder()
            .with_schema_id(0)
            .with_fields(vec![
                NestedField::required(1, "hour", Type::Primitive(PrimitiveType::Int)).into(),
                NestedField::required(2, "service_name", Type::Primitive(PrimitiveType::String))
                    .into(),
                NestedField::required(3, "tenant_id", Type::Primitive(PrimitiveType::String))
                    .into(),
            ])
            .build()
            .unwrap();
        let spec = layout_partition_spec(&schema, &RuntimeOutputLayout::default(), false)
            .unwrap()
            .unwrap();
        assert_eq!(spec.fields().len(), 3);
        assert!(spec
            .fields()
            .iter()
            .all(|field| field.transform == Transform::Identity));
    }

    #[test]
    fn unset_layout_without_otlp_is_unpartitioned() {
        let schema = Schema::builder()
            .with_schema_id(0)
            .with_fields(vec![NestedField::required(
                1,
                "id",
                Type::Primitive(PrimitiveType::Long),
            )
            .into()])
            .build()
            .unwrap();
        assert!(
            layout_partition_spec(&schema, &RuntimeOutputLayout::default(), false)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn live_identity_calendar_ints_do_not_match_day_layout() {
        let schema = Schema::builder()
            .with_schema_id(0)
            .with_fields(vec![
                NestedField::required(1, "event_ts", Type::Primitive(PrimitiveType::Timestamp))
                    .into(),
                NestedField::required(2, "year", Type::Primitive(PrimitiveType::Int)).into(),
                NestedField::required(3, "month", Type::Primitive(PrimitiveType::Int)).into(),
                NestedField::required(4, "day", Type::Primitive(PrimitiveType::Int)).into(),
            ])
            .build()
            .unwrap();
        let live = vec![
            (2, Transform::Identity),
            (3, Transform::Identity),
            (4, Transform::Identity),
        ];
        assert!(
            !live_spec_matches_layout(&schema, &live, &day_layout(&["event_ts"]), false).unwrap()
        );
    }

    #[test]
    fn posix_seconds_ms_and_rfc3339_resolve() {
        let posix = 1_794_063_845i64; // 2026-11-07 15:04:05 UTC
        let ms_batch = RecordBatch::try_from_iter(vec![(
            "time",
            Arc::new(Int64Array::from(vec![posix * 1000])) as ArrayRef,
        )])
        .unwrap();
        assert_eq!(
            event_time_posix_seconds(&ms_batch, &["time".into()], 0),
            Some(posix)
        );

        let us_batch = RecordBatch::try_from_iter(vec![(
            "time",
            Arc::new(Int64Array::from(vec![posix * 1_000_000])) as ArrayRef,
        )])
        .unwrap();
        assert_eq!(
            event_time_posix_seconds(&us_batch, &["time".into()], 0),
            Some(posix)
        );

        let keys = arrow::array::Int8Array::from(vec![0i8]);
        let values = Int64Array::from(vec![posix * 1000]);
        let dict = arrow::array::DictionaryArray::<arrow::datatypes::Int8Type>::try_new(
            keys,
            Arc::new(values),
        )
        .unwrap();
        let dict_batch =
            RecordBatch::try_from_iter(vec![("time", Arc::new(dict) as ArrayRef)]).unwrap();
        assert_eq!(
            event_time_posix_seconds(&dict_batch, &["time".into()], 0),
            Some(posix)
        );

        let ms_named = RecordBatch::try_from_iter(vec![(
            "event_ts",
            Arc::new(Int64Array::from(vec![posix * 1000])) as ArrayRef,
        )])
        .unwrap();
        assert_eq!(
            event_time_posix_seconds(&ms_named, &["event_ts".into()], 0),
            Some(posix)
        );

        let rfc_batch = RecordBatch::try_from_iter(vec![(
            "event_ts",
            Arc::new(StringArray::from(vec!["2026-11-07T15:04:05Z"])) as ArrayRef,
        )])
        .unwrap();
        assert_eq!(
            event_time_posix_seconds(&rfc_batch, &["event_ts".into()], 0),
            Some(posix)
        );

        let ts_batch = RecordBatch::try_from_iter(vec![(
            "event_ts",
            Arc::new(TimestampMillisecondArray::from(vec![posix * 1000])) as ArrayRef,
        )])
        .unwrap();
        assert_eq!(
            event_time_posix_seconds(&ts_batch, &["event_ts".into()], 0),
            Some(posix)
        );

        let decimal = Decimal128Array::from(vec![posix as i128])
            .with_precision_and_scale(38, 0)
            .unwrap();
        let dec_batch = RecordBatch::try_new(
            Arc::new(ArrowSchema::new(vec![Field::new(
                "event_ts",
                DataType::Decimal128(38, 0),
                false,
            )])),
            vec![Arc::new(decimal) as ArrayRef],
        )
        .unwrap();
        assert_eq!(
            event_time_posix_seconds(&dec_batch, &["event_ts".into()], 0),
            Some(posix)
        );
    }

    #[test]
    fn flattened_dotted_time_field_name_resolves() {
        let batch = RecordBatch::try_from_iter(vec![(
            "system_datetime_posix_utc_seconds",
            Arc::new(Int64Array::from(vec![1_794_063_845i64])) as ArrayRef,
        )])
        .unwrap();
        assert_eq!(
            event_time_posix_seconds(&batch, &["system.datetime_posix_utc_seconds".into()], 0),
            Some(1_794_063_845)
        );
    }

    #[test]
    fn explicit_posix_zero_is_a_resolvable_event_time() {
        let batch = RecordBatch::try_from_iter(vec![(
            "_skippr_event_time",
            Arc::new(Int64Array::from(vec![0i64])) as ArrayRef,
        )])
        .unwrap();
        assert_eq!(
            event_time_posix_seconds(&batch, &["_skippr_event_time".into()], 0),
            Some(0)
        );
    }

    #[test]
    fn cleaned_skipprd_event_time_column_resolves_configured_underscore_name() {
        let batch = RecordBatch::try_from_iter(vec![(
            "skippr_event_time",
            Arc::new(Int64Array::from(vec![1_794_063_845i64])) as ArrayRef,
        )])
        .unwrap();
        assert_eq!(
            event_time_posix_seconds(&batch, &["_skippr_event_time".into()], 0),
            Some(1_794_063_845),
            "Iceberg compact sees skipprd-cleaned column names"
        );
    }

    #[test]
    fn stamp_does_not_materialize_calendar_ints() {
        let posix = 1_794_063_845i64;
        let batch = RecordBatch::try_from_iter(vec![(
            "event_ts",
            Arc::new(Int64Array::from(vec![posix])) as ArrayRef,
        )])
        .unwrap();
        let schema = ts_schema("event_ts");
        let stamped =
            stamp_time_partition_columns(batch, &day_layout(&["event_ts"]), &schema).unwrap();
        assert!(stamped.column_by_name("year").is_none());
        assert!(stamped.column_by_name("month").is_none());
        assert!(stamped.column_by_name("day").is_none());
        assert!(matches!(
            stamped
                .schema()
                .field_with_name("event_ts")
                .unwrap()
                .data_type(),
            DataType::Timestamp(TimeUnit::Microsecond, _)
        ));
    }

    #[test]
    fn stamp_fails_closed_on_missing_time() {
        let batch = RecordBatch::try_from_iter(vec![(
            "event_ts",
            Arc::new(Int64Array::from(vec![None::<i64>])) as ArrayRef,
        )])
        .unwrap();
        let schema = ts_schema("event_ts");
        let err =
            stamp_time_partition_columns(batch, &day_layout(&["event_ts"]), &schema).unwrap_err();
        assert!(err.to_string().contains("batch_time_fields [event_ts]"));
    }

    #[test]
    fn stamp_fails_when_table_lacks_time_source() {
        let batch = RecordBatch::try_from_iter(vec![(
            "event_ts",
            Arc::new(Int64Array::from(vec![1_794_063_845i64])) as ArrayRef,
        )])
        .unwrap();
        let schema = Schema::builder()
            .with_schema_id(0)
            .with_fields(vec![NestedField::required(
                1,
                "id",
                Type::Primitive(PrimitiveType::Long),
            )
            .into()])
            .build()
            .unwrap();
        let err =
            stamp_time_partition_columns(batch, &day_layout(&["event_ts"]), &schema).unwrap_err();
        assert!(err.to_string().contains("recreate the table"));
    }
}
