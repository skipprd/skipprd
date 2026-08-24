use std::any::Any;
use std::collections::BTreeMap;
use std::sync::Arc;

use datafusion::arrow::array::RecordBatch;
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::catalog::{Session, TableFunctionImpl};
use datafusion::datasource::{TableProvider, TableType};
use datafusion::error::DataFusionError;
use datafusion::logical_expr::Expr;
use datafusion::physical_plan::ExecutionPlan;
use datafusion::prelude::SessionContext;

use super::budget::ScanBudget;
use super::catalog::{
    bool_array, bool_field, budget_time_filters, cap_and_set_truncated, collect_filtered_batches,
    expr_opt_i64, expr_utf8, i64_array, i64_field, i64_row, memory_scan, schema, utf8_array,
    utf8_field, utf8_row,
};
use super::trace::spans_schema;
use super::{MAX_EDGES, MAX_SERVICES};

fn services_schema() -> SchemaRef {
    schema(vec![
        utf8_field("service_name", false),
        i64_field("last_seen_unix_nano", false),
        i64_field("error_spans", false),
        bool_field("truncated", false),
    ])
}

fn service_map_schema() -> SchemaRef {
    schema(vec![
        utf8_field("from_service", false),
        utf8_field("to_service", false),
        bool_field("truncated", false),
    ])
}

#[derive(Debug)]
struct OtelServicesFn;

#[derive(Debug)]
struct ServicesTable {
    filters: Vec<Expr>,
}

#[async_trait::async_trait]
impl TableProvider for ServicesTable {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        services_schema()
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
        let mut by_service: BTreeMap<String, (i64, i64)> = BTreeMap::new();
        for batch in &batches {
            for row in 0..batch.num_rows() {
                let Some(name) = utf8_row(batch, "service_name", row) else {
                    continue;
                };
                let seen = i64_row(batch, "start_time_unix_nano", row).unwrap_or(0);
                let err = utf8_row(batch, "status_code", row) == Some("STATUS_CODE_ERROR");
                let entry = by_service.entry(name.to_string()).or_insert((seen, 0));
                if seen > entry.0 {
                    entry.0 = seen;
                }
                if err {
                    entry.1 = entry.1.saturating_add(1);
                }
            }
        }
        let names: Vec<String> = by_service.keys().cloned().collect();
        let last_seen: Vec<Option<i64>> = names.iter().map(|n| Some(by_service[n].0)).collect();
        let errors: Vec<Option<i64>> = names.iter().map(|n| Some(by_service[n].1)).collect();
        let n = names.len();
        let batch = RecordBatch::try_new(
            services_schema(),
            vec![
                utf8_array(names.iter().map(|s| Some(s.as_str())).collect()),
                i64_array(last_seen),
                i64_array(errors),
                bool_array(vec![false; n]),
            ],
        )
        .map_err(|e| DataFusionError::ArrowError(Box::new(e), None))?;
        let capped = cap_and_set_truncated(vec![batch], &services_schema(), MAX_SERVICES)?;
        memory_scan(state, services_schema(), capped, projection).await
    }
}

impl TableFunctionImpl for OtelServicesFn {
    fn call(&self, args: &[Expr]) -> datafusion::error::Result<Arc<dyn TableProvider>> {
        if args.len() < 2 || args.len() > 3 {
            return Err(DataFusionError::Plan(
                "otel_services(from_ns, to_ns [, tenant_id])".into(),
            ));
        }
        let budget = ScanBudget::new(
            expr_opt_i64(&args[0])?,
            expr_opt_i64(&args[1])?,
            if args.len() == 3 {
                Some(expr_utf8(&args[2])?)
            } else {
                None
            },
        )?;
        Ok(Arc::new(ServicesTable {
            filters: budget_time_filters("start_time_unix_nano", &budget),
        }))
    }
}

#[derive(Debug)]
struct OtelServiceMapFn;

#[derive(Debug)]
struct ServiceMapTable {
    service: String,
    filters: Vec<Expr>,
}

#[async_trait::async_trait]
impl TableProvider for ServiceMapTable {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        service_map_schema()
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
        let mut id_to_service: BTreeMap<String, String> = BTreeMap::new();
        let mut parent_of: Vec<(String, Option<String>, String)> = Vec::new();
        for batch in &batches {
            for row in 0..batch.num_rows() {
                let Some(span_id) = utf8_row(batch, "span_id", row) else {
                    continue;
                };
                let service = utf8_row(batch, "service_name", row)
                    .unwrap_or("")
                    .to_string();
                id_to_service.insert(span_id.to_string(), service.clone());
                parent_of.push((
                    span_id.to_string(),
                    utf8_row(batch, "parent_span_id", row).map(str::to_string),
                    service,
                ));
            }
        }
        let mut edges: BTreeMap<(String, String), ()> = BTreeMap::new();
        for (_id, parent, child_svc) in parent_of {
            let Some(parent_id) = parent else {
                continue;
            };
            let Some(from) = id_to_service.get(&parent_id) else {
                continue;
            };
            if from == &child_svc {
                continue;
            }
            if from != &self.service && child_svc != self.service {
                continue;
            }
            edges.insert((from.clone(), child_svc), ());
        }
        let froms: Vec<String> = edges.keys().map(|(a, _)| a.clone()).collect();
        let tos: Vec<String> = edges.keys().map(|(_, b)| b.clone()).collect();
        let n = froms.len();
        let batch = RecordBatch::try_new(
            service_map_schema(),
            vec![
                utf8_array(froms.iter().map(|s| Some(s.as_str())).collect()),
                utf8_array(tos.iter().map(|s| Some(s.as_str())).collect()),
                bool_array(vec![false; n]),
            ],
        )
        .map_err(|e| DataFusionError::ArrowError(Box::new(e), None))?;
        let capped = cap_and_set_truncated(vec![batch], &service_map_schema(), MAX_EDGES)?;
        memory_scan(state, service_map_schema(), capped, projection).await
    }
}

impl TableFunctionImpl for OtelServiceMapFn {
    fn call(&self, args: &[Expr]) -> datafusion::error::Result<Arc<dyn TableProvider>> {
        if args.len() < 3 || args.len() > 4 {
            return Err(DataFusionError::Plan(
                "otel_service_map(service, from_ns, to_ns [, tenant_id])".into(),
            ));
        }
        let service = expr_utf8(&args[0])?;
        let budget = ScanBudget::new(
            expr_opt_i64(&args[1])?,
            expr_opt_i64(&args[2])?,
            if args.len() == 4 {
                Some(expr_utf8(&args[3])?)
            } else {
                None
            },
        )?;
        Ok(Arc::new(ServiceMapTable {
            service,
            filters: budget_time_filters("start_time_unix_nano", &budget),
        }))
    }
}

pub fn register(ctx: &SessionContext) {
    ctx.register_udtf("otel_services", Arc::new(OtelServicesFn));
    ctx.register_udtf("otel_service_map", Arc::new(OtelServiceMapFn));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlrt::session::build_query_context;
    use crate::sqlrt::udfs::trace::spans_schema;
    use datafusion::arrow::array::{BooleanArray, Int64Array, StringArray, UInt32Array};
    use datafusion::datasource::MemTable;
    use datafusion::prelude::SessionConfig;

    fn two_service_spans() -> RecordBatch {
        let schema = spans_schema();
        RecordBatch::try_new(
            schema,
            vec![
                Arc::new(StringArray::from(vec!["aa", "aa"])),
                Arc::new(StringArray::from(vec!["s1", "s2"])),
                Arc::new(StringArray::from(vec![None::<&str>, Some("s1")])),
                Arc::new(StringArray::from(vec!["root", "child"])),
                Arc::new(StringArray::from(vec![
                    "SPAN_KIND_SERVER",
                    "SPAN_KIND_CLIENT",
                ])),
                Arc::new(Int64Array::from(vec![1, 2])),
                Arc::new(Int64Array::from(vec![3, 4])),
                Arc::new(Int64Array::from(vec![2, 2])),
                Arc::new(StringArray::from(vec![
                    "STATUS_CODE_ERROR",
                    "STATUS_CODE_OK",
                ])),
                Arc::new(StringArray::from(vec!["checkout", "payments"])),
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
    async fn two_services_and_one_edge() {
        let schema = spans_schema();
        let ctx = build_query_context(SessionConfig::new());
        ctx.register_table(
            "spans",
            Arc::new(MemTable::try_new(schema, vec![vec![two_service_spans()]]).unwrap()),
        )
        .unwrap();
        let services = ctx
            .sql("SELECT service_name, error_spans FROM otel_services(0, 10) ORDER BY service_name")
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert_eq!(services[0].num_rows(), 2);
        let edges = ctx
            .sql("SELECT from_service, to_service FROM otel_service_map('checkout', 0, 10)")
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert_eq!(edges.iter().map(|b| b.num_rows()).sum::<usize>(), 1);
        let unknown = ctx
            .sql("SELECT * FROM otel_service_map('missing', 0, 10)")
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert_eq!(unknown.iter().map(|b| b.num_rows()).sum::<usize>(), 0);
    }
}
