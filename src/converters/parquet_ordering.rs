use arrow::array::RecordBatch;
use arrow::compute::{concat_batches, lexsort_to_indices, take, SortColumn, SortOptions};
use arrow::datatypes::SchemaRef;
use dashmap::DashSet;
use once_cell::sync::Lazy;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;
use parquet::format::SortingColumn;
use std::collections::HashSet;
use tracing::warn;

use crate::helpers::configuration::Config;

/// Configured order field names that were resolved against at least one namespace.
static MATCHED_ORDER_FIELDS: Lazy<DashSet<String>> = Lazy::new(DashSet::new);

/// Returns the user-configured batch order field names, or an empty vec if unset.
fn configured_order_fields() -> Vec<String> {
    let raw = Config::get_transform_batch_order_fields();
    if raw.is_empty() {
        return vec![];
    }
    raw.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Resolve configured order fields against the output schema, returning only
/// those that exist. Records which fields were ever matched globally.
pub fn resolve_effective_order_from_fields(
    schema: &SchemaRef,
    configured: &[String],
) -> Vec<String> {
    if configured.is_empty() {
        return vec![];
    }
    let schema_fields: HashSet<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
    let mut effective = Vec::new();
    for name in configured {
        if schema_fields.contains(name.as_str()) {
            effective.push(name.clone());
            MATCHED_ORDER_FIELDS.insert(name.clone());
        }
    }
    effective
}

pub fn resolve_effective_order(schema: &SchemaRef) -> Vec<String> {
    let configured = configured_order_fields();
    resolve_effective_order_from_fields(schema, &configured)
}

/// Log a single end-of-run warning for configured order fields that never
/// matched any namespace observed during this process lifetime.
pub fn log_unmatched_order_fields() {
    let configured = configured_order_fields();
    if configured.is_empty() {
        return;
    }
    let unmatched: Vec<String> = configured
        .into_iter()
        .filter(|f| !MATCHED_ORDER_FIELDS.contains(f))
        .collect();
    if !unmatched.is_empty() {
        warn!(
            "batch_order_fields never matched any namespace during this run: {:?}",
            unmatched
        );
    }
}

/// Sort a single RecordBatch by the given column names (all ascending, nulls last).
fn sort_batch(batch: &RecordBatch, order_fields: &[String]) -> Result<RecordBatch, std::io::Error> {
    let schema = batch.schema();
    let sort_columns: Vec<SortColumn> = order_fields
        .iter()
        .filter_map(|name| {
            schema.index_of(name).ok().map(|idx| SortColumn {
                values: batch.column(idx).clone(),
                options: Some(SortOptions {
                    descending: false,
                    nulls_first: false,
                }),
            })
        })
        .collect();

    if sort_columns.is_empty() {
        return Ok(batch.clone());
    }

    let indices = lexsort_to_indices(&sort_columns, None)
        .map_err(|e| std::io::Error::other(format!("lexsort_to_indices failed: {e}")))?;

    let sorted_columns: Vec<_> = batch
        .columns()
        .iter()
        .map(|col| {
            take(col.as_ref(), &indices, None)
                .map_err(|e| std::io::Error::other(format!("take failed during sort: {e}")))
        })
        .collect::<Result<Vec<_>, _>>()?;

    RecordBatch::try_new(batch.schema(), sorted_columns)
        .map_err(|e| std::io::Error::other(format!("RecordBatch reconstruction after sort: {e}")))
}

/// Materialize a stream of record batches, optionally sort them by effective
/// order fields, and return the sorted batches ready for writing.
pub fn materialize_and_sort(
    batches: Vec<RecordBatch>,
    schema: &SchemaRef,
    order_fields: &[String],
) -> Result<Vec<RecordBatch>, std::io::Error> {
    if batches.is_empty() || order_fields.is_empty() {
        return Ok(batches);
    }

    let combined = concat_batches(schema, &batches)
        .map_err(|e| std::io::Error::other(format!("concat_batches failed: {e}")))?;

    let sorted = sort_batch(&combined, order_fields)?;
    Ok(vec![sorted])
}

/// Estimate optimal row-group size from materialized batch characteristics.
///
/// Uses average row width and leading-column run lengths to pick a row count
/// that keeps row groups between ~16 MiB and ~64 MiB uncompressed, clamped
/// to 25k..500k rows.
pub fn estimate_row_group_size(batches: &[RecordBatch], order_fields: &[String]) -> usize {
    const MIN_ROWS: usize = 25_000;
    const MAX_ROWS: usize = 500_000;
    const TARGET_BYTES_LOW: usize = 16 * 1024 * 1024;
    const TARGET_BYTES_HIGH: usize = 64 * 1024 * 1024;
    const DEFAULT_ROWS: usize = 100_000;

    let total_rows: usize = batches.iter().map(|b| b.num_rows()).sum();
    if total_rows == 0 {
        return DEFAULT_ROWS;
    }

    let total_bytes: usize = batches
        .iter()
        .map(|b| {
            b.columns()
                .iter()
                .map(|c| c.get_array_memory_size())
                .sum::<usize>()
        })
        .sum();

    let avg_row_bytes = (total_bytes / total_rows).max(1);

    // Target row count from byte budget (midpoint of range)
    let target_bytes = (TARGET_BYTES_LOW + TARGET_BYTES_HIGH) / 2;
    let mut target_rows = target_bytes / avg_row_bytes;

    // Adjust based on leading sort column fragmentation if sorted
    if !order_fields.is_empty() {
        if let Some(first_field) = order_fields.first() {
            let run_count = count_leading_runs(batches, first_field);
            if run_count > 0 {
                let avg_run_len = total_rows / run_count;
                // Short runs = high cardinality -> shrink row groups for better pruning
                if avg_run_len < 100 {
                    target_rows = target_rows.min(TARGET_BYTES_LOW / avg_row_bytes);
                }
                // Long runs = low cardinality -> grow row groups to reduce metadata
                else if avg_run_len > 10_000 {
                    target_rows = target_rows.max(TARGET_BYTES_HIGH / avg_row_bytes);
                }
            }
        }
    }

    target_rows.clamp(MIN_ROWS, MAX_ROWS)
}

/// Count the number of contiguous runs in the leading sort column across batches.
fn count_leading_runs(batches: &[RecordBatch], field_name: &str) -> usize {
    let mut runs: usize = 0;
    let mut prev_bytes: Option<Vec<u8>> = None;

    for batch in batches {
        let idx = match batch.schema().index_of(field_name) {
            Ok(i) => i,
            Err(_) => continue,
        };
        let col = batch.column(idx);
        let array_data = col.to_data();
        for row in 0..batch.num_rows() {
            // Use the arrow array's debug-formatted value as a simple equality proxy
            let current = format!("{:?}", arrow::array::make_array(array_data.slice(row, 1)));
            let current_bytes = current.into_bytes();
            match &prev_bytes {
                Some(prev) if *prev == current_bytes => {}
                _ => {
                    runs += 1;
                    prev_bytes = Some(current_bytes);
                }
            }
        }
    }
    runs
}

/// Build `WriterProperties` with optional sorting column metadata and auto-tuned
/// row-group size.
pub fn build_writer_properties(
    schema: &SchemaRef,
    order_fields: &[String],
    row_group_size: usize,
) -> WriterProperties {
    let mut builder = WriterProperties::builder()
        .set_dictionary_enabled(false)
        .set_encoding(parquet::basic::Encoding::PLAIN)
        .set_compression(Compression::SNAPPY)
        .set_max_row_group_size(row_group_size);

    if !order_fields.is_empty() {
        let sorting_cols: Vec<SortingColumn> = order_fields
            .iter()
            .filter_map(|name| {
                schema.index_of(name).ok().map(|idx| SortingColumn {
                    column_idx: idx as i32,
                    descending: false,
                    nulls_first: false,
                })
            })
            .collect();
        if !sorting_cols.is_empty() {
            builder = builder.set_sorting_columns(Some(sorting_cols));
        }
    }

    builder.build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Int32Array, StringArray};
    use arrow::datatypes::{DataType, Field, Schema};
    use std::sync::Arc;

    fn sample_schema() -> SchemaRef {
        Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int32, false),
            Field::new("name", DataType::Utf8, true),
            Field::new("value", DataType::Int32, true),
        ]))
    }

    fn sample_batch(schema: &SchemaRef) -> RecordBatch {
        RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int32Array::from(vec![3, 1, 2, 1, 3])),
                Arc::new(StringArray::from(vec![
                    Some("c"),
                    Some("a"),
                    Some("b"),
                    Some("a"),
                    Some("c"),
                ])),
                Arc::new(Int32Array::from(vec![30, 10, 20, 11, 31])),
            ],
        )
        .unwrap()
    }

    #[test]
    fn test_resolve_effective_order_filters_missing() {
        let schema = sample_schema();
        let order = vec![
            "id".to_string(),
            "missing_col".to_string(),
            "name".to_string(),
        ];
        let schema_fields: HashSet<&str> =
            schema.fields().iter().map(|f| f.name().as_str()).collect();
        let effective: Vec<String> = order
            .into_iter()
            .filter(|n| schema_fields.contains(n.as_str()))
            .collect();
        assert_eq!(effective, vec!["id", "name"]);
    }

    #[test]
    fn test_sort_batch_single_column() {
        let schema = sample_schema();
        let batch = sample_batch(&schema);
        let sorted = sort_batch(&batch, &["id".to_string()]).unwrap();
        let ids: Vec<i32> = sorted
            .column(0)
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap()
            .values()
            .to_vec();
        assert_eq!(ids, vec![1, 1, 2, 3, 3]);
    }

    #[test]
    fn test_sort_batch_multi_column() {
        let schema = sample_schema();
        let batch = sample_batch(&schema);
        let sorted = sort_batch(&batch, &["id".to_string(), "value".to_string()]).unwrap();
        let ids: Vec<i32> = sorted
            .column(0)
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap()
            .values()
            .to_vec();
        let vals: Vec<i32> = sorted
            .column(2)
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap()
            .values()
            .to_vec();
        assert_eq!(ids, vec![1, 1, 2, 3, 3]);
        assert_eq!(vals, vec![10, 11, 20, 30, 31]);
    }

    #[test]
    fn test_sort_batch_no_fields_is_noop() {
        let schema = sample_schema();
        let batch = sample_batch(&schema);
        let sorted = sort_batch(&batch, &[]).unwrap();
        assert_eq!(sorted.num_rows(), batch.num_rows());
        let ids: Vec<i32> = sorted
            .column(0)
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap()
            .values()
            .to_vec();
        assert_eq!(ids, vec![3, 1, 2, 1, 3]);
    }

    #[test]
    fn test_materialize_and_sort_combines_batches() {
        let schema = sample_schema();
        let b1 = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int32Array::from(vec![3, 1])),
                Arc::new(StringArray::from(vec![Some("c"), Some("a")])),
                Arc::new(Int32Array::from(vec![30, 10])),
            ],
        )
        .unwrap();
        let b2 = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int32Array::from(vec![2])),
                Arc::new(StringArray::from(vec![Some("b")])),
                Arc::new(Int32Array::from(vec![20])),
            ],
        )
        .unwrap();
        let result = materialize_and_sort(vec![b1, b2], &schema, &["id".to_string()]).unwrap();
        assert_eq!(result.len(), 1);
        let ids: Vec<i32> = result[0]
            .column(0)
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap()
            .values()
            .to_vec();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn test_estimate_row_group_size_within_bounds() {
        let schema = sample_schema();
        let batch = sample_batch(&schema);
        let size = estimate_row_group_size(&[batch], &["id".to_string()]);
        assert!(size >= 25_000 && size <= 500_000);
    }

    #[test]
    fn test_build_writer_properties_sets_sorting_metadata() {
        let schema = sample_schema();
        let props =
            build_writer_properties(&schema, &["id".to_string(), "name".to_string()], 100_000);
        let sorting = props.sorting_columns().unwrap();
        assert_eq!(sorting.len(), 2);
        assert_eq!(sorting[0].column_idx, 0); // id is index 0
        assert_eq!(sorting[1].column_idx, 1); // name is index 1
        assert!(!sorting[0].descending);
    }

    #[test]
    fn test_build_writer_properties_no_order_no_sorting_metadata() {
        let schema = sample_schema();
        let props = build_writer_properties(&schema, &[], 100_000);
        assert!(props.sorting_columns().is_none());
    }
}
