use datafusion::arrow::array::{
    Array, ArrayRef, BooleanArray, Int64Array, RecordBatch, StringArray,
};
use datafusion::arrow::compute::concat_batches;
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use datafusion::catalog::Session;
use datafusion::common::DFSchema;
use datafusion::datasource::{MemTable, TableProvider, TableType};
use datafusion::error::DataFusionError;
use datafusion::execution::session_state::SessionState;
use datafusion::logical_expr::utils::conjunction;
use datafusion::logical_expr::{col, lit, Expr};
use datafusion::physical_expr::create_physical_expr;
use datafusion::physical_expr::execution_props::ExecutionProps;
use datafusion::physical_plan::collect;
use datafusion::physical_plan::filter::FilterExec;
use datafusion::physical_plan::ExecutionPlan;
use datafusion::scalar::ScalarValue;
use std::any::Any;
use std::sync::Arc;

use super::budget::ScanBudget;
use super::cap_with_truncated;

pub async fn lookup_table(
    state: &dyn Session,
    name: &str,
) -> datafusion::error::Result<Arc<dyn TableProvider>> {
    let session_state = state
        .as_any()
        .downcast_ref::<SessionState>()
        .ok_or_else(|| DataFusionError::Internal("expected SessionState".into()))?;
    let cat_list = session_state.catalog_list();
    for catalog_name in cat_list.catalog_names() {
        let Some(catalog) = cat_list.catalog(&catalog_name) else {
            continue;
        };
        for schema_name in catalog.schema_names() {
            let Some(schema) = catalog.schema(&schema_name) else {
                continue;
            };
            if let Some(table) = schema.table(name).await? {
                return Ok(table);
            }
        }
    }
    Err(DataFusionError::Plan(format!(
        "observability table '{name}' is not registered"
    )))
}

pub fn utf8_lit(value: &str) -> Expr {
    lit(value)
}

pub fn i64_lit(value: i64) -> Expr {
    lit(value)
}

pub fn budget_time_filters(time_column: &str, budget: &ScanBudget) -> Vec<Expr> {
    let hour_from = hour_from_unix_nano(budget.from_unix_nano);
    let hour_to = hour_from_unix_nano(budget.to_unix_nano);
    let mut filters = vec![
        col(time_column).gt_eq(i64_lit(budget.from_unix_nano)),
        col(time_column).lt_eq(i64_lit(budget.to_unix_nano)),
        col("hour").gt_eq(lit(ScalarValue::UInt32(Some(hour_from)))),
        col("hour").lt_eq(lit(ScalarValue::UInt32(Some(hour_to)))),
    ];
    if let Some(tenant_id) = &budget.tenant.tenant_id {
        filters.push(col("tenant_id").eq(utf8_lit(tenant_id)));
    }
    filters
}

fn hour_from_unix_nano(ns: i64) -> u32 {
    (ns.max(0) as u64 / 3_600_000_000_000) as u32
}

#[derive(Debug)]
pub struct FilteredTable {
    pub table_name: String,
    pub schema: SchemaRef,
    pub filters: Vec<Expr>,
    pub limit: Option<usize>,
}

#[async_trait::async_trait]
impl TableProvider for FilteredTable {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn table_type(&self) -> TableType {
        TableType::View
    }

    async fn scan(
        &self,
        state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> datafusion::error::Result<Arc<dyn ExecutionPlan>> {
        let mut merged = self.filters.clone();
        merged.extend_from_slice(filters);
        let cap = limit.or(self.limit).unwrap_or(usize::MAX);
        let batches =
            collect_filtered_batches(state, &self.table_name, &merged, &self.schema).await?;
        let capped = cap_and_set_truncated(batches, &self.schema, cap)?;
        memory_scan(state, self.schema.clone(), capped, projection).await
    }
}

pub async fn collect_filtered_batches(
    state: &dyn Session,
    table_name: &str,
    filters: &[Expr],
    target: &SchemaRef,
) -> datafusion::error::Result<Vec<RecordBatch>> {
    let inner = lookup_table(state, table_name).await?;
    let mut plan = inner.scan(state, None, &[], None).await?;
    if let Some(pred) = conjunction(filters.to_vec()) {
        let df_schema = DFSchema::try_from(plan.schema())?;
        let phys = create_physical_expr(&pred, &df_schema, &ExecutionProps::new())?;
        plan = Arc::new(FilterExec::try_new(phys, plan)?);
    }
    let session_state = state
        .as_any()
        .downcast_ref::<SessionState>()
        .ok_or_else(|| DataFusionError::Internal("expected SessionState".into()))?;
    let batches = collect(plan, session_state.task_ctx()).await?;
    project_to_schema(&batches, target)
}

pub async fn memory_scan(
    state: &dyn Session,
    schema: SchemaRef,
    batches: Vec<RecordBatch>,
    projection: Option<&Vec<usize>>,
) -> datafusion::error::Result<Arc<dyn ExecutionPlan>> {
    let partitions = if batches.is_empty() {
        vec![vec![]]
    } else {
        vec![batches]
    };
    let table = MemTable::try_new(schema, partitions)?;
    table.scan(state, projection, &[], None).await
}

fn project_to_schema(
    batches: &[RecordBatch],
    target: &SchemaRef,
) -> datafusion::error::Result<Vec<RecordBatch>> {
    let mut out = Vec::with_capacity(batches.len());
    for batch in batches {
        let mut cols: Vec<ArrayRef> = Vec::with_capacity(target.fields().len());
        for field in target.fields() {
            if let Ok(idx) = batch.schema().index_of(field.name()) {
                cols.push(batch.column(idx).clone());
            } else if field.name() == "truncated" {
                cols.push(Arc::new(BooleanArray::from(vec![false; batch.num_rows()])) as ArrayRef);
            } else {
                return Err(DataFusionError::Plan(format!(
                    "observability column '{}' is not on scanned table",
                    field.name()
                )));
            }
        }
        out.push(RecordBatch::try_new(target.clone(), cols)?);
    }
    Ok(out)
}

pub fn cap_and_set_truncated(
    batches: Vec<RecordBatch>,
    schema: &SchemaRef,
    cap: usize,
) -> datafusion::error::Result<Vec<RecordBatch>> {
    if batches.is_empty() || batches.iter().all(|b| b.num_rows() == 0) {
        return Ok(Vec::new());
    }
    let combined = concat_batches(schema, &batches)?;
    let (take, truncated) = cap_with_truncated(combined.num_rows(), cap);
    let sliced = combined.slice(0, take);
    Ok(vec![set_truncated_column(sliced, truncated)?])
}

fn set_truncated_column(
    batch: RecordBatch,
    truncated: bool,
) -> datafusion::error::Result<RecordBatch> {
    let Ok(idx) = batch.schema().index_of("truncated") else {
        return Ok(batch);
    };
    let mut cols = batch.columns().to_vec();
    cols[idx] = Arc::new(BooleanArray::from(vec![truncated; batch.num_rows()]));
    Ok(RecordBatch::try_new(batch.schema(), cols)?)
}

pub fn utf8_row<'a>(batch: &'a RecordBatch, name: &str, row: usize) -> Option<&'a str> {
    let col = batch.column_by_name(name)?;
    let arr = col.as_any().downcast_ref::<StringArray>()?;
    if arr.is_null(row) {
        None
    } else {
        Some(arr.value(row))
    }
}

pub fn i64_row(batch: &RecordBatch, name: &str, row: usize) -> Option<i64> {
    let col = batch.column_by_name(name)?;
    let arr = col.as_any().downcast_ref::<Int64Array>()?;
    if arr.is_null(row) {
        None
    } else {
        Some(arr.value(row))
    }
}

pub fn utf8_array(values: Vec<Option<&str>>) -> ArrayRef {
    Arc::new(StringArray::from(values))
}

pub fn i64_array(values: Vec<Option<i64>>) -> ArrayRef {
    Arc::new(Int64Array::from(values))
}

pub fn bool_array(values: Vec<bool>) -> ArrayRef {
    Arc::new(BooleanArray::from(values))
}

pub fn utf8_field(name: &str, nullable: bool) -> Field {
    Field::new(name, DataType::Utf8, nullable)
}

pub fn i64_field(name: &str, nullable: bool) -> Field {
    Field::new(name, DataType::Int64, nullable)
}

pub fn u32_field(name: &str, nullable: bool) -> Field {
    Field::new(name, DataType::UInt32, nullable)
}

pub fn bool_field(name: &str, nullable: bool) -> Field {
    Field::new(name, DataType::Boolean, nullable)
}

pub fn ts_field(name: &str, nullable: bool) -> Field {
    Field::new(
        name,
        DataType::Timestamp(TimeUnit::Nanosecond, None),
        nullable,
    )
}

pub fn schema(fields: Vec<Field>) -> SchemaRef {
    Arc::new(Schema::new(fields))
}

pub fn expr_utf8(expr: &Expr) -> datafusion::error::Result<String> {
    match expr {
        Expr::Literal(scalar, _) => scalar
            .try_as_str()
            .flatten()
            .map(|s| s.to_string())
            .ok_or_else(|| DataFusionError::Plan(format!("expected utf8 literal, got {expr}"))),
        _ => Err(DataFusionError::Plan(format!(
            "expected utf8 literal, got {expr}"
        ))),
    }
}

pub fn expr_i64(expr: &Expr) -> datafusion::error::Result<i64> {
    match expr {
        Expr::Literal(ScalarValue::Int64(Some(v)), _) => Ok(*v),
        Expr::Literal(ScalarValue::Int32(Some(v)), _) => Ok(i64::from(*v)),
        Expr::Literal(ScalarValue::UInt64(Some(v)), _) => Ok(*v as i64),
        Expr::Literal(other, _) => Err(DataFusionError::Plan(format!(
            "expected int literal, got {other}"
        ))),
        _ => Err(DataFusionError::Plan(format!(
            "expected int literal, got {expr}"
        ))),
    }
}

pub fn expr_opt_utf8(expr: &Expr) -> datafusion::error::Result<Option<String>> {
    match expr {
        Expr::Literal(scalar, _) if scalar.is_null() => Ok(None),
        _ => Ok(Some(expr_utf8(expr)?)),
    }
}

pub fn expr_opt_i64(expr: &Expr) -> datafusion::error::Result<Option<i64>> {
    match expr {
        Expr::Literal(scalar, _) if scalar.is_null() => Ok(None),
        _ => Ok(Some(expr_i64(expr)?)),
    }
}

pub fn expr_bool(expr: &Expr) -> datafusion::error::Result<bool> {
    match expr {
        Expr::Literal(ScalarValue::Boolean(Some(v)), _) => Ok(*v),
        _ => Err(DataFusionError::Plan(format!(
            "expected bool literal, got {expr}"
        ))),
    }
}
