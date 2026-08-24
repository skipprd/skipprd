use std::sync::Arc;

use datafusion::catalog::TableFunctionImpl;
use datafusion::datasource::TableProvider;
use datafusion::error::DataFusionError;
use datafusion::logical_expr::{col, Expr};
use datafusion::prelude::SessionContext;

use super::budget::ScanBudget;
use super::catalog::{
    budget_time_filters, expr_opt_i64, expr_opt_utf8, expr_utf8, i64_field, schema, utf8_field,
    utf8_lit, FilteredTable,
};
use super::MAX_LOG_ROWS;

pub(crate) fn logs_schema() -> datafusion::arrow::datatypes::SchemaRef {
    schema(vec![
        i64_field("time_unix_nano", false),
        i64_field("observed_time_unix_nano", true),
        utf8_field("severity_text", true),
        super::catalog::i64_field("severity_number", true),
        utf8_field("body", false),
        utf8_field("trace_id", true),
        utf8_field("span_id", true),
        utf8_field("service_name", false),
        utf8_field("tenant_id", false),
        super::catalog::u32_field("hour", false),
        utf8_field("resource_attributes", false),
        utf8_field("log_attributes", false),
        super::catalog::bool_field("truncated", true),
    ])
}

#[derive(Debug)]
struct OtelLogsTailFn;

impl TableFunctionImpl for OtelLogsTailFn {
    fn call(&self, args: &[Expr]) -> datafusion::error::Result<Arc<dyn TableProvider>> {
        if args.len() < 3 || args.len() > 5 {
            return Err(DataFusionError::Plan(
                "otel_logs_tail(service, from_ns, to_ns [, body_filter [, tenant_id]])".into(),
            ));
        }
        let service = expr_utf8(&args[0])?;
        let budget = ScanBudget::new(
            expr_opt_i64(&args[1])?,
            expr_opt_i64(&args[2])?,
            if args.len() == 5 {
                expr_opt_utf8(&args[4])?
            } else {
                None
            },
        )?;
        let mut filters = budget_time_filters("time_unix_nano", &budget);
        filters.push(col("service_name").eq(utf8_lit(&service)));
        if args.len() >= 4 {
            if let Some(needle) = expr_opt_utf8(&args[3])? {
                filters.push(col("body").like(utf8_lit(&format!("%{needle}%"))));
            }
        }
        Ok(Arc::new(FilteredTable {
            table_name: "log_records".into(),
            schema: logs_schema(),
            filters,
            limit: Some(MAX_LOG_ROWS),
        }))
    }
}

#[derive(Debug)]
struct OtelLogsForTraceFn;

impl TableFunctionImpl for OtelLogsForTraceFn {
    fn call(&self, args: &[Expr]) -> datafusion::error::Result<Arc<dyn TableProvider>> {
        if args.len() < 3 || args.len() > 4 {
            return Err(DataFusionError::Plan(
                "otel_logs_for_trace(trace_id, from_ns, to_ns [, tenant_id])".into(),
            ));
        }
        let trace_id = expr_utf8(&args[0])?;
        let budget = ScanBudget::new(
            expr_opt_i64(&args[1])?,
            expr_opt_i64(&args[2])?,
            if args.len() == 4 {
                Some(expr_utf8(&args[3])?)
            } else {
                None
            },
        )?;
        let mut filters = budget_time_filters("time_unix_nano", &budget);
        filters.push(col("trace_id").eq(utf8_lit(&trace_id)));
        Ok(Arc::new(FilteredTable {
            table_name: "log_records".into(),
            schema: logs_schema(),
            filters,
            limit: Some(MAX_LOG_ROWS),
        }))
    }
}

pub fn register(ctx: &SessionContext) {
    ctx.register_udtf("otel_logs_tail", Arc::new(OtelLogsTailFn));
    ctx.register_udtf("otel_logs_for_trace", Arc::new(OtelLogsForTraceFn));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlrt::session::build_query_context;
    use datafusion::arrow::array::{Int64Array, RecordBatch, StringArray, UInt32Array};
    use datafusion::datasource::MemTable;
    use datafusion::prelude::SessionConfig;
    use std::sync::Arc;

    #[tokio::test]
    async fn logs_tail_and_trace() {
        let schema = logs_schema();
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(vec![5, 6])),
                Arc::new(Int64Array::from(vec![None, None])),
                Arc::new(StringArray::from(vec![Some("ERROR"), Some("INFO")])),
                Arc::new(Int64Array::from(vec![Some(17), Some(9)])),
                Arc::new(StringArray::from(vec!["timeout", "ok"])),
                Arc::new(StringArray::from(vec![Some("aa"), None])),
                Arc::new(StringArray::from(vec![Some("s1"), None])),
                Arc::new(StringArray::from(vec!["checkout", "checkout"])),
                Arc::new(StringArray::from(vec!["t", "t"])),
                Arc::new(UInt32Array::from(vec![0, 0])),
                Arc::new(StringArray::from(vec!["{}", "{}"])),
                Arc::new(StringArray::from(vec!["{}", "{}"])),
                Arc::new(datafusion::arrow::array::BooleanArray::from(vec![
                    false, false,
                ])),
            ],
        )
        .unwrap();
        let ctx = build_query_context(SessionConfig::new());
        ctx.register_table(
            "log_records",
            Arc::new(MemTable::try_new(schema, vec![vec![batch]]).unwrap()),
        )
        .unwrap();
        let tail = ctx
            .sql("SELECT * FROM otel_logs_tail('checkout', 0, 10, 'timeout')")
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert_eq!(tail[0].num_rows(), 1);
        let by_trace = ctx
            .sql("SELECT * FROM otel_logs_for_trace('aa', 0, 10)")
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert_eq!(by_trace[0].num_rows(), 1);
        let empty = ctx
            .sql("SELECT * FROM otel_logs_tail('other', 0, 10, NULL)")
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert_eq!(empty.iter().map(|b| b.num_rows()).sum::<usize>(), 0);
    }
}
