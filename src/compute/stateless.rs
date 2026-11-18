//! `apply_log` then ingest Stateless (filter remaps CDC; derive forbids row-count change).
//! One path including empty batches.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use arrow::array::{Array, BooleanArray, RecordBatch, StringArray};
use arrow::compute::filter_record_batch;
use arrow_schema::{DataType, Schema};
use datafusion::prelude::{SessionConfig, SessionContext};
use serde::{Deserialize, Serialize};

use crate::discover::schema_alter::{apply_log, CommittedAlter};
use crate::plugins::cdc::WalRowMeta;

use super::AlignedBatch;

static TABLE_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct StatelessSelect {
    pub filter: Option<String>,
    pub derive: Vec<(String, String)>,
}

impl StatelessSelect {
    pub fn is_empty(&self) -> bool {
        self.filter.is_none() && self.derive.is_empty()
    }
}

pub fn dest_schema(
    log: &[CommittedAlter],
    select: Option<&StatelessSelect>,
    source: Arc<Schema>,
) -> Result<Arc<Schema>, String> {
    let empty = RecordBatch::new_empty(source);
    let aligned = apply_stateless(log, select, empty, None)?;
    Ok(aligned.batch.schema())
}

/// Drop rows whose event-time column is null or unparsable. Caller deadletters `dropped`.
fn event_time_column_index(schema: &Schema, col: &str) -> Result<usize, String> {
    let trimmed = col.trim();
    let mut names = vec![trimmed.to_string()];
    let stripped = trimmed.trim_matches('_');
    if stripped != trimmed && !stripped.is_empty() {
        names.push(stripped.to_string());
    }
    for name in &names {
        if let Ok(idx) = schema.index_of(name) {
            return Ok(idx);
        }
    }
    schema.index_of(trimmed).map_err(|err| err.to_string())
}

pub fn drop_invalid_event_times(
    batch: RecordBatch,
    cdc: Option<Vec<WalRowMeta>>,
    col: &str,
) -> Result<(AlignedBatch, usize), String> {
    let idx = event_time_column_index(batch.schema().as_ref(), col)?;
    let keep = event_time_keep(batch.column(idx))?;
    let dropped = keep.iter().filter(|keep| !**keep).count();
    if dropped == 0 {
        return Ok((AlignedBatch::new(batch, cdc)?, 0));
    }
    let mask = BooleanArray::from(keep.clone());
    let filtered = filter_record_batch(&batch, &mask).map_err(|err| err.to_string())?;
    let cdc = match cdc {
        None => None,
        Some(rows) => Some(
            rows.into_iter()
                .zip(keep)
                .filter_map(|(row, keep)| keep.then_some(row))
                .collect(),
        ),
    };
    Ok((AlignedBatch::new(filtered, cdc)?, dropped))
}

fn event_time_keep(array: &dyn Array) -> Result<Vec<bool>, String> {
    match array.data_type() {
        DataType::Timestamp(_, _)
        | DataType::Date32
        | DataType::Date64
        | DataType::Time64(_)
        | DataType::Int64
        | DataType::Int32 => Ok((0..array.len()).map(|i| array.is_valid(i)).collect()),
        DataType::Utf8 => {
            let arr = array
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| "event_time utf8 column".to_string())?;
            Ok((0..arr.len())
                .map(|i| {
                    if arr.is_null(i) {
                        false
                    } else {
                        chrono::DateTime::parse_from_rfc3339(arr.value(i)).is_ok()
                    }
                })
                .collect())
        }
        other => Err(format!("event_time column type {other} is not a timestamp")),
    }
}

pub fn apply_stateless(
    log: &[CommittedAlter],
    select: Option<&StatelessSelect>,
    batch: RecordBatch,
    cdc: Option<Vec<WalRowMeta>>,
) -> Result<AlignedBatch, String> {
    let batch = if log.is_empty() {
        batch
    } else {
        apply_log(batch, log).map_err(|err| err.to_string())?
    };
    let Some(select) = select.filter(|s| !s.is_empty()) else {
        return AlignedBatch::new(batch, cdc);
    };
    let mut current = batch;
    let mut kept: Option<Vec<usize>> = None;
    if let Some(expr) = select.filter.as_deref() {
        let before = current.num_rows();
        let (filtered, mask) = apply_filter_with_mask(current, expr)?;
        kept = Some(compose_kept(kept.take(), before, &mask));
        current = filtered;
    }
    let rows_before_derive = current.num_rows();
    for (name, expr) in &select.derive {
        if name.trim().is_empty() {
            return Err("derive column name must be non-empty".into());
        }
        let table = unique_table();
        let sql = format!("SELECT *, ({expr}) AS {} FROM {table}", quote_ident(name));
        current = run_sql(current, &table, &sql)?;
        if current.num_rows() != rows_before_derive {
            return Err("derive cannot change row count".into());
        }
    }
    let cdc = match (cdc, kept.as_ref()) {
        (None, _) => None,
        (Some(rows), None) => Some(rows),
        (Some(rows), Some(kept)) => {
            if rows.len() < kept.iter().copied().max().map(|i| i + 1).unwrap_or(0) {
                return Err(format!(
                    "filter cannot realign CDC sidecar: {} rows, kept index out of range",
                    rows.len()
                ));
            }
            Some(kept.iter().map(|&i| rows[i].clone()).collect())
        }
    };
    AlignedBatch::new(current, cdc)
}

fn compose_kept(previous: Option<Vec<usize>>, before: usize, mask: &[bool]) -> Vec<usize> {
    let base: Vec<usize> = previous.unwrap_or_else(|| (0..before).collect());
    base.into_iter()
        .zip(mask.iter().copied())
        .filter_map(|(idx, keep)| keep.then_some(idx))
        .collect()
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
    let flag_batch = run_sql(batch.clone(), &table, &sql)?;
    let col = flag_batch
        .column(0)
        .as_any()
        .downcast_ref::<BooleanArray>()
        .ok_or_else(|| "filter expression must be boolean".to_string())?;
    if col.len() != batch.num_rows() {
        return Err(format!(
            "filter returned {} flags for {} rows",
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

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn unique_table() -> String {
    format!("b{}", TABLE_SEQ.fetch_add(1, Ordering::Relaxed))
}

fn run_sql(batch: RecordBatch, table: &str, sql: &str) -> Result<RecordBatch, String> {
    let batches = block_on_df(async move {
        let ctx = SessionContext::new_with_config(SessionConfig::new().with_target_partitions(1));
        ctx.register_batch(table, batch)
            .map_err(|err| err.to_string())?;
        let df = ctx.sql(sql).await.map_err(|err| err.to_string())?;
        df.collect().await.map_err(|err| err.to_string())
    })?;
    if batches.is_empty() {
        return Err("SQL produced no batch".into());
    }
    if batches.len() == 1 {
        return Ok(batches.into_iter().next().unwrap());
    }
    let schema = batches[0].schema();
    arrow::compute::concat_batches(&schema, &batches).map_err(|err| err.to_string())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::cdc::MutationKind;
    use arrow::array::{Int64Array, StringArray};
    use arrow_schema::{DataType, Field, Schema};

    fn invoice_batch(customers: &[&str], amounts: &[i64]) -> RecordBatch {
        RecordBatch::try_new(
            Arc::new(Schema::new(vec![
                Field::new("customer_id", DataType::Utf8, false),
                Field::new("amount", DataType::Int64, false),
            ])),
            vec![
                Arc::new(StringArray::from(customers.to_vec())),
                Arc::new(Int64Array::from(amounts.to_vec())),
            ],
        )
        .unwrap()
    }

    #[test]
    fn filter_keeps_cdc_sidecar() {
        let batch = invoice_batch(&["a", "b", "c"], &[0, 5, 0]);
        let cdc = vec![
            WalRowMeta::new(MutationKind::Insert, b"a".to_vec(), vec![1]),
            WalRowMeta::new(MutationKind::Insert, b"b".to_vec(), vec![2]),
            WalRowMeta::new(MutationKind::Insert, b"c".to_vec(), vec![3]),
        ];
        let select = StatelessSelect {
            filter: Some("amount > 0".into()),
            derive: Vec::new(),
        };
        let aligned = apply_stateless(&[], Some(&select), batch, Some(cdc)).unwrap();
        assert_eq!(aligned.batch.num_rows(), 1);
        assert_eq!(aligned.cdc.as_ref().unwrap()[0].event_id, b"b");
    }

    #[test]
    fn empty_batch_still_schema() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("customer_id", DataType::Utf8, false),
            Field::new("amount", DataType::Int64, false),
        ]));
        let select = StatelessSelect {
            filter: Some("amount > 0".into()),
            derive: vec![("double".into(), "amount * 2".into())],
        };
        let aligned = apply_stateless(
            &[],
            Some(&select),
            RecordBatch::new_empty(schema.clone()),
            Some(Vec::new()),
        )
        .unwrap();
        assert_eq!(aligned.batch.num_rows(), 0);
        assert!(aligned.batch.schema().field_with_name("double").is_ok());
    }

    #[test]
    fn derive_cannot_change_row_count_is_enforced() {
        let batch = invoice_batch(&["a"], &[3]);
        let select = StatelessSelect {
            filter: None,
            derive: vec![("amount2".into(), "amount + 1".into())],
        };
        let aligned = apply_stateless(&[], Some(&select), batch, None).unwrap();
        assert_eq!(aligned.batch.num_rows(), 1);
        assert!(aligned.batch.schema().field_with_name("amount2").is_ok());
    }

    #[test]
    fn drop_invalid_event_times_resolves_cleaned_skippr_column() {
        let batch = RecordBatch::try_from_iter(vec![(
            "skippr_event_time",
            Arc::new(Int64Array::from(vec![1_794_063_845i64])) as arrow::array::ArrayRef,
        )])
        .unwrap();
        let (aligned, dropped) =
            drop_invalid_event_times(batch, None, "_skippr_event_time").unwrap();
        assert_eq!(dropped, 0);
        assert_eq!(aligned.batch.num_rows(), 1);
    }
}
