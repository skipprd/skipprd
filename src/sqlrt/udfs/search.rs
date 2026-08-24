use std::any::Any;
use std::sync::Arc;

use datafusion::arrow::array::RecordBatch;
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::catalog::{Session, TableFunctionImpl};
use datafusion::datasource::{TableProvider, TableType};
use datafusion::error::DataFusionError;
use datafusion::logical_expr::{col, lit, Expr};
use datafusion::physical_plan::ExecutionPlan;
use datafusion::prelude::SessionContext;

use super::budget::ScanBudget;
use super::catalog::{
    bool_array, bool_field, budget_time_filters, cap_and_set_truncated, collect_filtered_batches,
    expr_bool, expr_opt_i64, expr_opt_utf8, expr_utf8, i64_array, i64_field, i64_row, memory_scan,
    schema, utf8_array, utf8_field, utf8_lit, utf8_row,
};
use super::trace::spans_schema;
use super::MAX_TRACES;

fn search_schema() -> SchemaRef {
    schema(vec![
        utf8_field("trace_id", false),
        utf8_field("root_name", false),
        i64_field("start_time_unix_nano", false),
        i64_field("end_time_unix_nano", false),
        i64_field("duration_nano", false),
        bool_field("error", false),
        bool_field("truncated", false),
    ])
}

#[derive(Debug)]
struct OtelTraceSearchFn;

#[derive(Debug)]
struct SearchTable {
    min_duration_nano: Option<i64>,
    filters: Vec<Expr>,
}

#[async_trait::async_trait]
impl TableProvider for SearchTable {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        search_schema()
    }

    fn table_type(&self) -> TableType {
        TableType::View
    }

    async fn scan(
        &self,
        state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        _limit: Option<usize>,
    ) -> datafusion::error::Result<Arc<dyn ExecutionPlan>> {
        let mut merged = self.filters.clone();
        merged.extend_from_slice(filters);
        let batches = collect_filtered_batches(state, "spans", &merged, &spans_schema()).await?;
        let mut rows: Vec<(String, String, i64, i64, i64, bool)> = Vec::new();
        for batch in &batches {
            for row in 0..batch.num_rows() {
                let duration = i64_row(batch, "duration_nano", row).unwrap_or(0);
                if let Some(min) = self.min_duration_nano {
                    if duration < min {
                        continue;
                    }
                }
                rows.push((
                    utf8_row(batch, "trace_id", row).unwrap_or("").to_string(),
                    utf8_row(batch, "name", row).unwrap_or("").to_string(),
                    i64_row(batch, "start_time_unix_nano", row).unwrap_or(0),
                    i64_row(batch, "end_time_unix_nano", row).unwrap_or(0),
                    duration,
                    utf8_row(batch, "status_code", row) == Some("STATUS_CODE_ERROR"),
                ));
            }
        }
        rows.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
        let n = rows.len();
        let batch = RecordBatch::try_new(
            search_schema(),
            vec![
                utf8_array(rows.iter().map(|r| Some(r.0.as_str())).collect()),
                utf8_array(rows.iter().map(|r| Some(r.1.as_str())).collect()),
                i64_array(rows.iter().map(|r| Some(r.2)).collect()),
                i64_array(rows.iter().map(|r| Some(r.3)).collect()),
                i64_array(rows.iter().map(|r| Some(r.4)).collect()),
                bool_array(rows.iter().map(|r| r.5).collect()),
                bool_array(vec![false; n]),
            ],
        )
        .map_err(|e| DataFusionError::ArrowError(Box::new(e), None))?;
        let capped = cap_and_set_truncated(vec![batch], &search_schema(), MAX_TRACES)?;
        memory_scan(state, search_schema(), capped, projection).await
    }
}

impl TableFunctionImpl for OtelTraceSearchFn {
    fn call(&self, args: &[Expr]) -> datafusion::error::Result<Arc<dyn TableProvider>> {
        if args.len() < 3 || args.len() > 6 {
            return Err(DataFusionError::Plan(
                "otel_trace_search(service, from_ns, to_ns [, error_only [, min_duration_nano [, tenant_id]]])"
                    .into(),
            ));
        }
        let service = expr_utf8(&args[0])?;
        let error_only = if args.len() >= 4 {
            expr_bool(&args[3]).unwrap_or(false)
        } else {
            false
        };
        let (min_duration_nano, tenant_id) = match args.len() {
            6 => (expr_opt_i64(&args[4])?, expr_opt_utf8(&args[5])?),
            5 => (None, expr_opt_utf8(&args[4])?),
            _ => (None, None),
        };
        let budget = ScanBudget::new(expr_opt_i64(&args[1])?, expr_opt_i64(&args[2])?, tenant_id)?;
        let mut filters = budget_time_filters("start_time_unix_nano", &budget);
        filters.push(col("service_name").eq(utf8_lit(&service)));
        filters.push(col("parent_span_id").is_null());
        if error_only {
            filters.push(col("status_code").eq(lit("STATUS_CODE_ERROR")));
        }
        Ok(Arc::new(SearchTable {
            min_duration_nano,
            filters,
        }))
    }
}

pub fn register(ctx: &SessionContext) {
    ctx.register_udtf("otel_trace_search", Arc::new(OtelTraceSearchFn));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlrt::session::build_query_context;
    use datafusion::arrow::array::{BooleanArray, Int64Array, StringArray, UInt32Array};
    use datafusion::datasource::MemTable;
    use datafusion::prelude::SessionConfig;

    fn spans_batch() -> RecordBatch {
        let schema = spans_schema();
        RecordBatch::try_new(
            schema,
            vec![
                Arc::new(StringArray::from(vec!["aa", "bb"])),
                Arc::new(StringArray::from(vec!["s1", "s2"])),
                Arc::new(StringArray::from(vec![None::<&str>, None])),
                Arc::new(StringArray::from(vec!["root", "other"])),
                Arc::new(StringArray::from(vec![
                    "SPAN_KIND_SERVER",
                    "SPAN_KIND_SERVER",
                ])),
                Arc::new(Int64Array::from(vec![1, 1])),
                Arc::new(Int64Array::from(vec![2, 2])),
                Arc::new(Int64Array::from(vec![1, 1])),
                Arc::new(StringArray::from(vec![
                    "STATUS_CODE_ERROR",
                    "STATUS_CODE_OK",
                ])),
                Arc::new(StringArray::from(vec!["checkout", "checkout"])),
                Arc::new(StringArray::from(vec![None::<&str>, None])),
                Arc::new(StringArray::from(vec!["t", "t"])),
                Arc::new(StringArray::from(vec![None::<&str>, None])),
                Arc::new(UInt32Array::from(vec![0, 0])),
                Arc::new(StringArray::from(vec!["{}", "{}"])),
                Arc::new(StringArray::from(vec!["{}", "{}"])),
                Arc::new(BooleanArray::from(vec![false, false])),
            ],
        )
        .unwrap()
    }

    #[tokio::test]
    async fn service_error_filter_and_empty() {
        let ctx = build_query_context(SessionConfig::new());
        ctx.register_table(
            "spans",
            Arc::new(MemTable::try_new(spans_schema(), vec![vec![spans_batch()]]).unwrap()),
        )
        .unwrap();
        let hits = ctx
            .sql("SELECT trace_id, root_name, error FROM otel_trace_search('checkout', 0, 10, true, NULL)")
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert_eq!(hits[0].num_rows(), 1);
        let empty = ctx
            .sql("SELECT * FROM otel_trace_search('missing', 0, 10, false)")
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert_eq!(empty.iter().map(|b| b.num_rows()).sum::<usize>(), 0);
    }
}
