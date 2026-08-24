use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use datafusion::arrow::array::RecordBatch;
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::catalog::{Session, TableFunctionImpl};
use datafusion::datasource::{TableProvider, TableType};
use datafusion::error::DataFusionError;
use datafusion::logical_expr::{col, Expr};
use datafusion::physical_plan::ExecutionPlan;
use datafusion::prelude::SessionContext;

use super::budget::ScanBudget;
use super::catalog::{
    bool_array, budget_time_filters, collect_filtered_batches, expr_opt_i64, expr_utf8, i64_array,
    i64_field, i64_row, memory_scan, schema, u32_field, utf8_array, utf8_field, utf8_lit, utf8_row,
};
use super::trace::spans_schema;
use super::MAX_TRACES;
use std::any::Any;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaterfallSpan {
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub name: String,
    pub service_name: String,
    pub start_time_unix_nano: i64,
    pub end_time_unix_nano: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaterfallRow {
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub depth: u32,
    pub duration_nano: i64,
    pub service_name: String,
    pub name: String,
    pub start_time_unix_nano: i64,
    pub end_time_unix_nano: i64,
    pub orphan: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaterfallError {
    Cycle(String),
}

impl std::fmt::Display for WaterfallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cycle(id) => write!(f, "otel_waterfall cycle at span {id}"),
        }
    }
}

impl std::error::Error for WaterfallError {}

impl From<WaterfallError> for DataFusionError {
    fn from(err: WaterfallError) -> Self {
        DataFusionError::Plan(err.to_string())
    }
}

pub fn assemble_waterfall(spans: &[WaterfallSpan]) -> Result<Vec<WaterfallRow>, WaterfallError> {
    let by_id: HashMap<&str, &WaterfallSpan> =
        spans.iter().map(|s| (s.span_id.as_str(), s)).collect();
    let mut children: HashMap<&str, Vec<&WaterfallSpan>> = HashMap::new();
    for span in spans {
        if let Some(parent) = span.parent_span_id.as_deref() {
            children.entry(parent).or_default().push(span);
        }
    }
    let mut out = Vec::with_capacity(spans.len());
    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();
    fn walk<'a>(
        span: &'a WaterfallSpan,
        depth: u32,
        orphan: bool,
        by_id: &HashMap<&str, &'a WaterfallSpan>,
        children: &HashMap<&str, Vec<&'a WaterfallSpan>>,
        visiting: &mut HashSet<&'a str>,
        visited: &mut HashSet<&'a str>,
        out: &mut Vec<WaterfallRow>,
    ) -> Result<(), WaterfallError> {
        if visiting.contains(span.span_id.as_str()) {
            return Err(WaterfallError::Cycle(span.span_id.clone()));
        }
        if !visited.insert(span.span_id.as_str()) {
            return Ok(());
        }
        visiting.insert(span.span_id.as_str());
        let duration_nano = span
            .end_time_unix_nano
            .saturating_sub(span.start_time_unix_nano);
        out.push(WaterfallRow {
            span_id: span.span_id.clone(),
            parent_span_id: span.parent_span_id.clone(),
            depth,
            duration_nano,
            service_name: span.service_name.clone(),
            name: span.name.clone(),
            start_time_unix_nano: span.start_time_unix_nano,
            end_time_unix_nano: span.end_time_unix_nano,
            orphan,
        });
        if let Some(kids) = children.get(span.span_id.as_str()) {
            for child in kids {
                walk(
                    child,
                    depth.saturating_add(1),
                    false,
                    by_id,
                    children,
                    visiting,
                    visited,
                    out,
                )?;
            }
        }
        visiting.remove(span.span_id.as_str());
        Ok(())
    }
    let mut roots: Vec<&WaterfallSpan> = spans
        .iter()
        .filter(|s| {
            s.parent_span_id
                .as_deref()
                .map(|p| !by_id.contains_key(p))
                .unwrap_or(true)
        })
        .collect();
    roots.sort_by_key(|s| s.start_time_unix_nano);
    for root in roots {
        let orphan = root.parent_span_id.is_some();
        walk(
            root,
            0,
            orphan,
            &by_id,
            &children,
            &mut visiting,
            &mut visited,
            &mut out,
        )?;
    }
    for span in spans {
        if visited.contains(span.span_id.as_str()) {
            continue;
        }
        walk(
            span,
            0,
            true,
            &by_id,
            &children,
            &mut visiting,
            &mut visited,
            &mut out,
        )?;
    }
    Ok(out)
}

fn waterfall_schema() -> SchemaRef {
    schema(vec![
        utf8_field("span_id", false),
        utf8_field("parent_span_id", true),
        u32_field("depth", false),
        i64_field("duration_nano", false),
        utf8_field("service_name", false),
        utf8_field("name", false),
        i64_field("start_time_unix_nano", false),
        i64_field("end_time_unix_nano", false),
        super::catalog::bool_field("orphan", false),
    ])
}

fn spans_from_batches(batches: &[RecordBatch]) -> Vec<WaterfallSpan> {
    let mut out = Vec::new();
    for batch in batches {
        for row in 0..batch.num_rows() {
            let Some(span_id) = utf8_row(batch, "span_id", row) else {
                continue;
            };
            out.push(WaterfallSpan {
                span_id: span_id.to_string(),
                parent_span_id: utf8_row(batch, "parent_span_id", row).map(str::to_string),
                name: utf8_row(batch, "name", row).unwrap_or("").to_string(),
                service_name: utf8_row(batch, "service_name", row)
                    .unwrap_or("")
                    .to_string(),
                start_time_unix_nano: i64_row(batch, "start_time_unix_nano", row).unwrap_or(0),
                end_time_unix_nano: i64_row(batch, "end_time_unix_nano", row).unwrap_or(0),
            });
        }
    }
    out
}

fn rows_to_batch(rows: &[WaterfallRow]) -> datafusion::error::Result<RecordBatch> {
    let schema = waterfall_schema();
    RecordBatch::try_new(
        schema,
        vec![
            utf8_array(rows.iter().map(|r| Some(r.span_id.as_str())).collect()),
            utf8_array(rows.iter().map(|r| r.parent_span_id.as_deref()).collect()),
            Arc::new(datafusion::arrow::array::UInt32Array::from(
                rows.iter().map(|r| r.depth).collect::<Vec<_>>(),
            )),
            i64_array(rows.iter().map(|r| Some(r.duration_nano)).collect()),
            utf8_array(rows.iter().map(|r| Some(r.service_name.as_str())).collect()),
            utf8_array(rows.iter().map(|r| Some(r.name.as_str())).collect()),
            i64_array(rows.iter().map(|r| Some(r.start_time_unix_nano)).collect()),
            i64_array(rows.iter().map(|r| Some(r.end_time_unix_nano)).collect()),
            bool_array(rows.iter().map(|r| r.orphan).collect()),
        ],
    )
    .map_err(|e| DataFusionError::ArrowError(Box::new(e), None))
}

#[derive(Debug)]
struct OtelWaterfallFn;

#[derive(Debug)]
struct WaterfallTable {
    filters: Vec<Expr>,
}

#[async_trait::async_trait]
impl TableProvider for WaterfallTable {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        waterfall_schema()
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
        let spans = spans_from_batches(&batches);
        let rows = assemble_waterfall(&spans)?;
        let capped: Vec<WaterfallRow> = rows.into_iter().take(MAX_TRACES).collect();
        let batch = rows_to_batch(&capped)?;
        memory_scan(state, waterfall_schema(), vec![batch], projection).await
    }
}

impl TableFunctionImpl for OtelWaterfallFn {
    fn call(&self, args: &[Expr]) -> datafusion::error::Result<Arc<dyn TableProvider>> {
        if args.len() < 3 || args.len() > 4 {
            return Err(DataFusionError::Plan(
                "otel_waterfall(trace_id, from_ns, to_ns [, tenant_id])".into(),
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
        let mut filters = budget_time_filters("start_time_unix_nano", &budget);
        filters.push(col("trace_id").eq(utf8_lit(&trace_id)));
        Ok(Arc::new(WaterfallTable { filters }))
    }
}

pub fn register(ctx: &SessionContext) {
    ctx.register_udtf("otel_waterfall", Arc::new(OtelWaterfallFn));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(id: &str, parent: Option<&str>, start: i64, end: i64) -> WaterfallSpan {
        WaterfallSpan {
            span_id: id.into(),
            parent_span_id: parent.map(str::to_string),
            name: id.into(),
            service_name: "checkout".into(),
            start_time_unix_nano: start,
            end_time_unix_nano: end,
        }
    }

    #[test]
    fn parent_child_depths() {
        let rows =
            assemble_waterfall(&[span("root", None, 0, 10), span("child", Some("root"), 1, 4)])
                .unwrap();
        assert_eq!(rows[0].span_id, "root");
        assert_eq!(rows[0].depth, 0);
        assert!(!rows[0].orphan);
        assert_eq!(rows[1].span_id, "child");
        assert_eq!(rows[1].depth, 1);
        assert_eq!(rows[1].duration_nano, 3);
    }

    #[test]
    fn orphan_emitted_at_depth_zero() {
        let rows = assemble_waterfall(&[span("orphan", Some("missing"), 0, 2)]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].depth, 0);
        assert!(rows[0].orphan);
    }

    #[test]
    fn cycle_fails_closed() {
        let err = assemble_waterfall(&[span("a", Some("b"), 0, 1), span("b", Some("a"), 0, 1)])
            .unwrap_err();
        assert!(matches!(err, WaterfallError::Cycle(_)));
    }
}
