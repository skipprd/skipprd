//! Optional in-memory Arrow ops applied after schema mapping and before WAL commit.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use arrow::array::{Array, ArrayRef, BooleanArray, RecordBatch};
use arrow::compute::{cast, filter_record_batch};
use arrow_schema::{DataType, Field, Schema, TimeUnit};
use datafusion::execution::context::SessionContext;

use crate::helpers::configuration::TransformOp;
use crate::plugins::cdc::WalRowMeta;

static TABLE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Result of applying `transform.ops` to Arrow batches.
#[derive(Debug)]
pub struct AppliedBatches {
    pub batches: Vec<RecordBatch>,
    /// Original row indices kept when any filter ran. `None` means row count unchanged.
    pub kept: Option<Vec<usize>>,
}

pub fn apply_configured_ops(
    ops: &[TransformOp],
    batches: Vec<RecordBatch>,
    cdc_rows: Option<Vec<WalRowMeta>>,
) -> Result<(Vec<RecordBatch>, Option<Vec<WalRowMeta>>), String> {
    if ops.is_empty() || batches.iter().all(|b| b.num_rows() == 0) {
        return Ok((batches, cdc_rows));
    }
    let applied = apply_ops(ops, batches)?;
    let cdc_rows = match (cdc_rows, applied.kept.as_ref()) {
        (None, _) => None,
        (Some(rows), None) => Some(rows),
        (Some(rows), Some(kept)) => {
            if rows.len() < kept.iter().copied().max().map(|i| i + 1).unwrap_or(0) {
                return Err(format!(
                    "transform.ops filter cannot realign CDC sidecar: {} rows, kept index out of range",
                    rows.len()
                ));
            }
            Some(kept.iter().map(|&i| rows[i].clone()).collect())
        }
    };
    Ok((applied.batches, cdc_rows))
}

pub fn apply_ops(ops: &[TransformOp], batches: Vec<RecordBatch>) -> Result<AppliedBatches, String> {
    let Some(first) = batches.first() else {
        return Ok(AppliedBatches {
            batches: Vec::new(),
            kept: None,
        });
    };
    let schema = first.schema();
    let merged = if batches.len() == 1 {
        first.clone()
    } else {
        arrow::compute::concat_batches(&schema, &batches).map_err(|err| err.to_string())?
    };
    crate::metrics::counters::add_stream_transform_records(merged.num_rows() as u64);
    let mut kept: Option<Vec<usize>> = None;
    let mut current = merged;
    for op in ops {
        op.validate()?;
        current = apply_one_filter_recording(op, current, &mut kept)?;
    }
    if let Some(indices) = kept.as_ref() {
        if indices.len() != current.num_rows() {
            return Err(format!(
                "transform.ops kept {} original rows but produced {} output rows",
                indices.len(),
                current.num_rows()
            ));
        }
    }
    let batches = if current.num_rows() == 0 {
        Vec::new()
    } else {
        vec![current]
    };
    Ok(AppliedBatches { batches, kept })
}

fn compose_kept(previous: Option<Vec<usize>>, before: usize, mask: &[bool]) -> Vec<usize> {
    let base: Vec<usize> = previous.unwrap_or_else(|| (0..before).collect());
    base.into_iter()
        .zip(mask.iter().copied())
        .filter_map(|(idx, keep)| keep.then_some(idx))
        .collect()
}

fn apply_one(op: &TransformOp, batch: RecordBatch) -> Result<RecordBatch, String> {
    if op.filter.is_some() {
        return Err("internal: filter ops must use apply_filter_with_mask".into());
    }
    if let Some(columns) = op.project.as_ref() {
        return apply_project(batch, columns);
    }
    if let Some(derive) = op.derive.as_ref() {
        return apply_derive(batch, derive);
    }
    if let Some(rename) = op.rename.as_ref() {
        return apply_rename(batch, rename);
    }
    if let Some(cast_map) = op.cast.as_ref() {
        return apply_cast(batch, cast_map);
    }
    Err("transform.ops entry must set exactly one of filter, project, derive, rename, cast".into())
}

fn apply_filter_with_mask(
    batch: RecordBatch,
    expr: &str,
) -> Result<(RecordBatch, Vec<bool>), String> {
    if batch.num_rows() == 0 {
        return Ok((batch, Vec::new()));
    }
    let table = unique_table();
    let sql = format!("SELECT ({expr}) AS __skippr_keep FROM {table}");
    let flags = run_sql(batch.clone(), &table, &sql)?;
    let flag_batch = flags
        .into_iter()
        .next()
        .ok_or_else(|| "transform.ops filter produced no boolean batch".to_string())?;
    let col = flag_batch
        .column(0)
        .as_any()
        .downcast_ref::<BooleanArray>()
        .ok_or_else(|| "transform.ops filter expression must be boolean".to_string())?;
    if col.len() != batch.num_rows() {
        return Err(format!(
            "transform.ops filter returned {} flags for {} rows",
            col.len(),
            batch.num_rows()
        ));
    }
    let mask: Vec<bool> = (0..col.len())
        .map(|i| col.is_valid(i) && col.value(i))
        .collect();
    let bool_array = BooleanArray::from(mask.clone());
    let filtered = filter_record_batch(&batch, &bool_array).map_err(|err| err.to_string())?;
    Ok((filtered, mask))
}

fn apply_project(batch: RecordBatch, columns: &[String]) -> Result<RecordBatch, String> {
    if columns.is_empty() {
        return Err("transform.ops project requires at least one column".into());
    }
    let mut fields = Vec::with_capacity(columns.len());
    let mut arrays: Vec<ArrayRef> = Vec::with_capacity(columns.len());
    for name in columns {
        let idx = batch
            .schema()
            .index_of(name)
            .map_err(|_| format!("transform.ops project unknown column '{name}'"))?;
        fields.push(batch.schema().field(idx).clone());
        arrays.push(batch.column(idx).clone());
    }
    RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays).map_err(|err| err.to_string())
}

fn apply_rename(
    batch: RecordBatch,
    rename: &BTreeMap<String, String>,
) -> Result<RecordBatch, String> {
    let mut fields = Vec::with_capacity(batch.num_columns());
    for field in batch.schema().fields() {
        if let Some(new_name) = rename.get(field.name()) {
            if new_name.trim().is_empty() {
                return Err(format!(
                    "transform.ops rename target for '{}' must be non-empty",
                    field.name()
                ));
            }
            fields.push(Field::new(
                new_name,
                field.data_type().clone(),
                field.is_nullable(),
            ));
        } else {
            fields.push(field.as_ref().clone());
        }
    }
    RecordBatch::try_new(Arc::new(Schema::new(fields)), batch.columns().to_vec())
        .map_err(|err| err.to_string())
}

fn apply_cast(batch: RecordBatch, casts: &BTreeMap<String, String>) -> Result<RecordBatch, String> {
    let mut fields = Vec::with_capacity(batch.num_columns());
    let mut arrays: Vec<ArrayRef> = Vec::with_capacity(batch.num_columns());
    for (idx, field) in batch.schema().fields().iter().enumerate() {
        if let Some(type_name) = casts.get(field.name()) {
            let dt = parse_arrow_type(type_name)?;
            let casted = cast(batch.column(idx), &dt).map_err(|err| {
                format!(
                    "transform.ops cast '{}' to {type_name}: {err}",
                    field.name()
                )
            })?;
            fields.push(Field::new(field.name(), dt, field.is_nullable()));
            arrays.push(casted);
        } else {
            fields.push(field.as_ref().clone());
            arrays.push(batch.column(idx).clone());
        }
    }
    RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays).map_err(|err| err.to_string())
}

fn apply_derive(
    batch: RecordBatch,
    derive: &BTreeMap<String, String>,
) -> Result<RecordBatch, String> {
    let mut current = batch;
    for (name, expr) in derive {
        if name.trim().is_empty() {
            return Err("transform.ops derive column name must be non-empty".into());
        }
        let table = unique_table();
        let sql = format!("SELECT *, ({expr}) AS {} FROM {table}", quote_ident(name));
        let out = run_sql(current, &table, &sql)?;
        current = out
            .into_iter()
            .next()
            .ok_or_else(|| format!("transform.ops derive '{name}' produced no batch"))?;
    }
    Ok(current)
}

fn parse_arrow_type(name: &str) -> Result<DataType, String> {
    match name.trim().to_ascii_lowercase().as_str() {
        "int8" => Ok(DataType::Int8),
        "int16" => Ok(DataType::Int16),
        "int32" => Ok(DataType::Int32),
        "int64" => Ok(DataType::Int64),
        "uint8" => Ok(DataType::UInt8),
        "uint16" => Ok(DataType::UInt16),
        "uint32" => Ok(DataType::UInt32),
        "uint64" => Ok(DataType::UInt64),
        "float32" => Ok(DataType::Float32),
        "float64" | "double" => Ok(DataType::Float64),
        "utf8" | "string" => Ok(DataType::Utf8),
        "bool" | "boolean" => Ok(DataType::Boolean),
        "date32" => Ok(DataType::Date32),
        "timestamp" | "timestamp_us" => Ok(DataType::Timestamp(TimeUnit::Microsecond, None)),
        other => Err(format!("transform.ops cast unknown type '{other}'")),
    }
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn unique_table() -> String {
    format!("b{}", TABLE_SEQ.fetch_add(1, Ordering::Relaxed))
}

fn run_sql(batch: RecordBatch, table: &str, sql: &str) -> Result<Vec<RecordBatch>, String> {
    block_on_df(async move {
        let ctx = SessionContext::new();
        ctx.register_batch(table, batch)
            .map_err(|err| err.to_string())?;
        let df = ctx.sql(sql).await.map_err(|err| err.to_string())?;
        df.collect().await.map_err(|err| err.to_string())
    })
}

fn block_on_df<F, T>(fut: F) -> Result<T, String>
where
    F: std::future::Future<Output = Result<T, String>>,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(fut)),
        Err(_) => {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|err| err.to_string())?;
            rt.block_on(fut)
        }
    }
}

fn apply_one_filter_recording(
    op: &TransformOp,
    batch: RecordBatch,
    kept: &mut Option<Vec<usize>>,
) -> Result<RecordBatch, String> {
    if let Some(expr) = op.filter.as_deref() {
        let before = batch.num_rows();
        let (filtered, mask) = apply_filter_with_mask(batch, expr)?;
        *kept = Some(compose_kept(kept.take(), before, &mask));
        return Ok(filtered);
    }
    apply_one(op, batch)
}

#[cfg(test)]
fn identity_kept(n: usize) -> Vec<usize> {
    (0..n).collect()
}

#[cfg(test)]
mod tests_fuzz;

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Float64Array, Int64Array, StringArray};
    use arrow_schema::Field;

    fn batch_amounts(ids: &[&str], amounts: &[i64]) -> RecordBatch {
        let schema = Schema::new(vec![
            Field::new("id", DataType::Utf8, false),
            Field::new("amount", DataType::Int64, false),
        ]);
        RecordBatch::try_new(
            Arc::new(schema),
            vec![
                Arc::new(StringArray::from(ids.to_vec())),
                Arc::new(Int64Array::from(amounts.to_vec())),
            ],
        )
        .unwrap()
    }

    #[test]
    fn default_off_is_identity() {
        let batch = batch_amounts(&["a", "b"], &[1, 2]);
        let applied = apply_ops(&[], vec![batch.clone()]).unwrap();
        assert_eq!(applied.batches[0], batch);
        assert!(applied.kept.is_none());
    }

    #[test]
    fn filter_project_derive_rename_cast_compose() {
        let batch = batch_amounts(&["a", "b", "c"], &[0, 10, 20]);
        let ops = vec![
            TransformOp {
                filter: Some("amount > 0".into()),
                ..Default::default()
            },
            TransformOp {
                derive: Some(
                    [("amount_gbp".into(), "amount * 0.8".into())]
                        .into_iter()
                        .collect(),
                ),
                ..Default::default()
            },
            TransformOp {
                project: Some(vec!["id".into(), "amount".into(), "amount_gbp".into()]),
                ..Default::default()
            },
            TransformOp {
                rename: Some(
                    [("amount".into(), "amount_usd".into())]
                        .into_iter()
                        .collect(),
                ),
                ..Default::default()
            },
            TransformOp {
                cast: Some(
                    [("amount_usd".into(), "float64".into())]
                        .into_iter()
                        .collect(),
                ),
                ..Default::default()
            },
        ];
        let applied = apply_ops(&ops, vec![batch]).unwrap();
        assert_eq!(applied.kept.as_deref(), Some(&[1usize, 2][..]));
        let out = &applied.batches[0];
        assert_eq!(
            out.schema()
                .fields()
                .iter()
                .map(|f| f.name().as_str())
                .collect::<Vec<_>>(),
            vec!["id", "amount_usd", "amount_gbp"]
        );
        assert_eq!(out.num_rows(), 2);
        let usd = out
            .column(1)
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap();
        assert_eq!(usd.value(0), 10.0);
        assert_eq!(usd.value(1), 20.0);
    }

    #[test]
    fn filter_realigns_cdc_sidecar() {
        let batch = batch_amounts(&["a", "b", "c"], &[0, 5, 0]);
        let cdc = vec![
            WalRowMeta {
                mutation: crate::plugins::cdc::MutationKind::Insert,
                event_id: b"a".to_vec(),
                order_token: vec![1],
            },
            WalRowMeta {
                mutation: crate::plugins::cdc::MutationKind::Insert,
                event_id: b"b".to_vec(),
                order_token: vec![2],
            },
            WalRowMeta {
                mutation: crate::plugins::cdc::MutationKind::Insert,
                event_id: b"c".to_vec(),
                order_token: vec![3],
            },
        ];
        let ops = vec![TransformOp {
            filter: Some("amount > 0".into()),
            ..Default::default()
        }];
        let (batches, cdc) = apply_configured_ops(&ops, vec![batch], Some(cdc)).unwrap();
        assert_eq!(batches[0].num_rows(), 1);
        let cdc = cdc.unwrap();
        assert_eq!(cdc.len(), 1);
        assert_eq!(cdc[0].event_id, b"b");
    }

    #[test]
    fn unknown_project_column_fails() {
        let batch = batch_amounts(&["a"], &[1]);
        let ops = vec![TransformOp {
            project: Some(vec!["missing".into()]),
            ..Default::default()
        }];
        let err = apply_ops(&ops, vec![batch]).unwrap_err();
        assert!(err.contains("unknown column"), "{err}");
    }

    #[test]
    fn schema_change_adds_derived_column() {
        let batch = batch_amounts(&["a"], &[10]);
        let ops = vec![TransformOp {
            derive: Some(
                [("amount_gbp".into(), "amount * 0.8".into())]
                    .into_iter()
                    .collect(),
            ),
            ..Default::default()
        }];
        let applied = apply_ops(&ops, vec![batch]).unwrap();
        let schema = applied.batches[0].schema();
        let names: Vec<_> = schema.fields().iter().map(|f| f.name().as_str()).collect();
        assert_eq!(names, vec!["id", "amount", "amount_gbp"]);
        let gbp = applied.batches[0]
            .column(2)
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap();
        assert_eq!(gbp.value(0), 8.0);
    }

    #[test]
    fn ops_failure_returns_error_without_batches() {
        let batch = batch_amounts(&["a"], &[1]);
        let ops = vec![TransformOp {
            filter: Some("not_a_column > 0".into()),
            ..Default::default()
        }];
        assert!(apply_configured_ops(&ops, vec![batch], None).is_err());
    }

    #[test]
    fn identity_kept_helper_covers_original_order() {
        assert_eq!(identity_kept(3), vec![0, 1, 2]);
    }
}
