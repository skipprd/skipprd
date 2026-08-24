use std::sync::Arc;

use datafusion::catalog::TableFunctionImpl;
use datafusion::datasource::TableProvider;
use datafusion::error::DataFusionError;
use datafusion::logical_expr::{col, Expr};
use datafusion::prelude::SessionContext;

use super::budget::ScanBudget;
use super::catalog::{
    bool_field, budget_time_filters, expr_opt_i64, expr_utf8, i64_field, schema, utf8_field,
    utf8_lit, FilteredTable,
};
use super::MAX_TRACES;

#[derive(Debug)]
struct OtelTraceFn;

impl TableFunctionImpl for OtelTraceFn {
    fn call(&self, args: &[Expr]) -> datafusion::error::Result<Arc<dyn TableProvider>> {
        if args.len() < 3 || args.len() > 4 {
            return Err(DataFusionError::Plan(
                "otel_trace(trace_id, from_ns, to_ns [, tenant_id])".into(),
            ));
        }
        let trace_id = expr_utf8(&args[0])?;
        let from_ns = expr_opt_i64(&args[1])?;
        let to_ns = expr_opt_i64(&args[2])?;
        let tenant_id = if args.len() == 4 {
            Some(expr_utf8(&args[3])?)
        } else {
            None
        };
        let budget = ScanBudget::new(from_ns, to_ns, tenant_id)?;
        let mut filters = budget_time_filters("start_time_unix_nano", &budget);
        filters.push(col("trace_id").eq(utf8_lit(&trace_id)));
        Ok(Arc::new(FilteredTable {
            table_name: "spans".into(),
            schema: spans_schema(),
            filters,
            limit: Some(MAX_TRACES),
        }))
    }
}

pub fn spans_schema() -> datafusion::arrow::datatypes::SchemaRef {
    schema(vec![
        utf8_field("trace_id", false),
        utf8_field("span_id", false),
        utf8_field("parent_span_id", true),
        utf8_field("name", false),
        utf8_field("kind", false),
        i64_field("start_time_unix_nano", false),
        i64_field("end_time_unix_nano", false),
        i64_field("duration_nano", false),
        utf8_field("status_code", false),
        utf8_field("service_name", false),
        utf8_field("deployment_environment", true),
        utf8_field("tenant_id", false),
        utf8_field("http_route", true),
        super::catalog::u32_field("hour", false),
        utf8_field("resource_attributes", false),
        utf8_field("span_attributes", false),
        bool_field("truncated", true),
    ])
}

pub fn register(ctx: &SessionContext) {
    ctx.register_udtf("otel_trace", Arc::new(OtelTraceFn));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlrt::session::build_query_context;
    use datafusion::arrow::array::{Int64Array, RecordBatch, StringArray};
    use datafusion::arrow::datatypes::Schema;
    use datafusion::datasource::MemTable;
    use datafusion::prelude::SessionConfig;
    use std::sync::Arc;

    #[tokio::test]
    async fn otel_trace_filters_by_id() {
        let schema = spans_schema();
        let batch = RecordBatch::try_new(
            schema.clone(),
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
                Arc::new(StringArray::from(vec!["STATUS_CODE_OK", "STATUS_CODE_OK"])),
                Arc::new(StringArray::from(vec!["checkout", "checkout"])),
                Arc::new(StringArray::from(vec![None::<&str>, None])),
                Arc::new(StringArray::from(vec!["t", "t"])),
                Arc::new(StringArray::from(vec![None::<&str>, None])),
                Arc::new(datafusion::arrow::array::UInt32Array::from(vec![0, 0])),
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
            "spans",
            Arc::new(MemTable::try_new(schema, vec![vec![batch]]).unwrap()),
        )
        .unwrap();
        let rows = ctx
            .sql("SELECT * FROM otel_trace('aa', 0, 10)")
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert_eq!(rows[0].num_rows(), 1);
        let unknown = ctx
            .sql("SELECT * FROM otel_trace('zz', 0, 10)")
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert_eq!(unknown.iter().map(|b| b.num_rows()).sum::<usize>(), 0);
        let search = ctx
            .sql("SELECT * FROM otel_trace_search('checkout', 0, 10, false)")
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert!(search.iter().map(|b| b.num_rows()).sum::<usize>() >= 1);
        let wf = ctx
            .sql("SELECT * FROM otel_waterfall('aa', 0, 10)")
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert_eq!(wf[0].num_rows(), 1);
        let services = ctx
            .sql("SELECT DISTINCT service_name FROM otel_services(0, 10)")
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert!(services.iter().map(|b| b.num_rows()).sum::<usize>() >= 1);
        let map = ctx
            .sql("SELECT from_service, to_service FROM otel_service_map('checkout', 0, 10)")
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        let _ = map;
        let _ = Schema::empty();
    }
}
