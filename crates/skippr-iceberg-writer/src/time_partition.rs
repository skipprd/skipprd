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
use iceberg::spec::Schema;
use skippr_runtime_sdk::helpers::Helpers;
use skippr_runtime_sdk::protocol::RuntimeOutputLayout;
use skippr_runtime_sdk::sink_compat::partition_time;

pub(crate) fn time_partition_column_names_from_layout(
    layout: &RuntimeOutputLayout,
) -> Result<Vec<String>, io::Error> {
    let Some(unit) = layout
        .time_partition_granularity
        .as_deref()
        .map(str::trim)
        .filter(|unit| !unit.is_empty())
    else {
        return Ok(Vec::new());
    };
    if layout.batch_time_fields.is_empty() {
        return Ok(Vec::new());
    }
    Ok(partition_time::time_partition_column_names(unit)?
        .into_iter()
        .map(str::to_string)
        .collect())
}

pub(crate) fn stamp_time_partition_columns(
    batch: RecordBatch,
    layout: &RuntimeOutputLayout,
    iceberg_schema: &Schema,
) -> Result<RecordBatch, io::Error> {
    let column_names = time_partition_column_names_from_layout(layout)?;
    if column_names.is_empty() {
        return Ok(batch);
    }
    let missing: Vec<&str> = column_names
        .iter()
        .filter(|name| iceberg_schema.field_by_name(name).is_none())
        .map(String::as_str)
        .collect();
    if !missing.is_empty() {
        return Err(io::Error::other(format!(
            "Iceberg table is missing time partition columns {}; recreate the table",
            missing.join(", ")
        )));
    }
    if batch.num_rows() == 0 {
        return attach_time_partition_columns(batch, &column_names, Vec::new());
    }
    let mut values: Vec<Vec<i32>> = column_names
        .iter()
        .map(|_| Vec::with_capacity(batch.num_rows()))
        .collect();
    for row in 0..batch.num_rows() {
        let posix =
            event_time_posix_seconds(&batch, &layout.batch_time_fields, row).ok_or_else(|| {
                io::Error::other(format!(
                    "missing or unresolvable event time in configured batch_time_fields [{}]",
                    layout.batch_time_fields.join(", ")
                ))
            })?;
        let unit = layout
            .time_partition_granularity
            .as_deref()
            .unwrap_or_default();
        let columns = partition_time::calendar_columns_utc(posix, unit)?;
        if columns.len() != column_names.len() {
            return Err(io::Error::other(
                "Iceberg time partition column count did not match the configured unit",
            ));
        }
        for (idx, (name, value)) in columns.into_iter().enumerate() {
            if name != column_names[idx] {
                return Err(io::Error::other(format!(
                    "Iceberg time partition column mismatch: expected {}, got {name}",
                    column_names[idx]
                )));
            }
            values[idx].push(value);
        }
    }
    attach_time_partition_columns(batch, &column_names, values)
}

fn attach_time_partition_columns(
    batch: RecordBatch,
    column_names: &[String],
    values: Vec<Vec<i32>>,
) -> Result<RecordBatch, io::Error> {
    let schema = batch.schema();
    let drop: std::collections::HashSet<&str> = column_names.iter().map(String::as_str).collect();
    let mut fields = Vec::new();
    let mut columns = Vec::new();
    for (field, column) in schema.fields().iter().zip(batch.columns()) {
        if drop.contains(field.name().as_str()) {
            continue;
        }
        fields.push(field.clone());
        columns.push(column.clone());
    }
    let empty = Vec::new();
    for (idx, name) in column_names.iter().enumerate() {
        let ints = values.get(idx).unwrap_or(&empty);
        fields.push(Arc::new(Field::new(name, DataType::Int32, false)));
        columns.push(Arc::new(Int32Array::from(ints.clone())) as arrow::array::ArrayRef);
    }
    RecordBatch::try_new(Arc::new(ArrowSchema::new(fields)), columns)
        .map_err(|err| io::Error::other(err.to_string()))
}

fn event_time_posix_seconds(batch: &RecordBatch, fields: &[String], row: usize) -> Option<i64> {
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
    let trimmed = field.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(idx) = schema.index_of(trimmed) {
        return Some(idx);
    }
    let flattened = trimmed.replace('.', "_");
    if flattened != trimmed {
        if let Ok(idx) = schema.index_of(&flattened) {
            return Some(idx);
        }
    }
    // skipprd `clean_field_name` strips leading/trailing underscores, so a
    // configured `_skippr_event_time` lands on Arrow/Iceberg as `skippr_event_time`.
    let stripped = trimmed.trim_matches('_');
    if stripped != trimmed && !stripped.is_empty() {
        if let Ok(idx) = schema.index_of(stripped) {
            return Some(idx);
        }
        let flattened_stripped = stripped.replace('.', "_");
        if flattened_stripped != stripped {
            return schema.index_of(&flattened_stripped).ok();
        }
    }
    None
}

fn posix_seconds_from_numeric_i64(time: i64) -> Option<i64> {
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
    use arrow::datatypes::Field;
    use iceberg::spec::{NestedField, PrimitiveType, Type};

    fn iceberg_with_time_columns(names: &[&str]) -> Schema {
        let fields: Vec<Arc<NestedField>> = names
            .iter()
            .enumerate()
            .map(|(idx, name)| {
                Arc::new(NestedField::required(
                    (idx as i32) + 10,
                    *name,
                    Type::Primitive(PrimitiveType::Int),
                ))
            })
            .collect();
        Schema::builder()
            .with_schema_id(0)
            .with_fields(fields)
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

    #[test]
    fn layout_without_unit_or_fields_is_disabled() {
        assert!(
            time_partition_column_names_from_layout(&RuntimeOutputLayout::default())
                .unwrap()
                .is_empty()
        );
        let unit_only = RuntimeOutputLayout {
            time_partition_granularity: Some("day".into()),
            ..RuntimeOutputLayout::default()
        };
        assert!(time_partition_column_names_from_layout(&unit_only)
            .unwrap()
            .is_empty());
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
    fn stamp_writes_utc_calendar_ints() {
        let posix = 1_794_063_845i64;
        let batch = RecordBatch::try_from_iter(vec![(
            "event_ts",
            Arc::new(Int64Array::from(vec![posix])) as ArrayRef,
        )])
        .unwrap();
        let schema = iceberg_with_time_columns(&["year", "month", "day"]);
        let stamped =
            stamp_time_partition_columns(batch, &day_layout(&["event_ts"]), &schema).unwrap();
        let year = stamped
            .column_by_name("year")
            .unwrap()
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap();
        let month = stamped
            .column_by_name("month")
            .unwrap()
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap();
        let day = stamped
            .column_by_name("day")
            .unwrap()
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap();
        assert_eq!(year.value(0), 2026);
        assert_eq!(month.value(0), 11);
        assert_eq!(day.value(0), 7);
    }

    #[test]
    fn stamp_fails_closed_on_missing_time() {
        let batch = RecordBatch::try_from_iter(vec![(
            "event_ts",
            Arc::new(Int64Array::from(vec![None::<i64>])) as ArrayRef,
        )])
        .unwrap();
        let schema = iceberg_with_time_columns(&["year", "month", "day"]);
        let err =
            stamp_time_partition_columns(batch, &day_layout(&["event_ts"]), &schema).unwrap_err();
        assert!(err.to_string().contains("batch_time_fields [event_ts]"));
    }

    #[test]
    fn stamp_fails_when_table_lacks_calendar_columns() {
        let batch = RecordBatch::try_from_iter(vec![(
            "event_ts",
            Arc::new(Int64Array::from(vec![1_794_063_845i64])) as ArrayRef,
        )])
        .unwrap();
        let schema = Schema::builder()
            .with_schema_id(0)
            .with_fields(vec![NestedField::required(
                1,
                "event_ts",
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
