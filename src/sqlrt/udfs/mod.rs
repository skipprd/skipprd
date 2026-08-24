//! Observability SQL UDFs/TVFs. MUST NOT mention `PIPELINE_NAME` or `get_pipeline_name`.

mod budget;
mod catalog;
mod histogram;
mod increase;
mod logs;
mod rate;
mod search;
mod services;
mod trace;
mod waterfall;

use datafusion::prelude::SessionContext;

pub use budget::{ScanBudget, ScanBudgetError, TenantFilter, OTEL_DEFAULT_LOOKBACK};
pub use histogram::OtelHistogramQuantile;
pub use increase::OtelIncrease;
pub use rate::OtelRate;

pub const MAX_SERIES: usize = 1000;
pub const MAX_TRACES: usize = 100;
pub const MAX_LOG_ROWS: usize = 1000;
pub const MAX_SERVICES: usize = 500;
pub const MAX_EDGES: usize = 200;

pub fn cap_with_truncated(count: usize, cap: usize) -> (usize, bool) {
    if count > cap {
        (cap, true)
    } else {
        (count, false)
    }
}

/// Deterministic top-N by `abs(value)`, then name (E.4).
pub fn top_n_by_abs(mut series: Vec<(String, f64)>, cap: usize) -> (Vec<(String, f64)>, bool) {
    series.sort_by(|a, b| {
        b.1.abs()
            .partial_cmp(&a.1.abs())
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    let (take, truncated) = cap_with_truncated(series.len(), cap);
    series.truncate(take);
    (series, truncated)
}

pub fn observability_udf_names() -> &'static [&'static str] {
    OBSERVABILITY_UDF_NAMES
}

pub const OBSERVABILITY_UDF_NAMES: &[&str] = &[
    "otel_trace",
    "otel_waterfall",
    "otel_trace_search",
    "otel_logs_tail",
    "otel_logs_for_trace",
    "otel_rate",
    "otel_increase",
    "otel_histogram_quantile",
    "otel_services",
    "otel_service_map",
];

pub fn register_observability_udfs(ctx: &SessionContext) {
    rate::register(ctx);
    increase::register(ctx);
    histogram::register(ctx);
    trace::register(ctx);
    waterfall::register(ctx);
    search::register(ctx);
    logs::register(ctx);
    services::register(ctx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    #[test]
    fn udfs_must_not_mention_pipeline_name() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/sqlrt/udfs");
        visit(&root);
    }

    fn visit(path: &Path) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                visit(&path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            if path.file_name().and_then(|n| n.to_str()) == Some("mod.rs") {
                continue;
            }
            let text = fs::read_to_string(&path).unwrap();
            assert!(
                !text.contains("PIPELINE_NAME"),
                "{} must not mention PIPELINE_NAME",
                path.display()
            );
            assert!(
                !text.contains("get_pipeline_name"),
                "{} must not mention get_pipeline_name",
                path.display()
            );
        }
    }

    #[test]
    fn missing_pipeline_is_typed_error() {
        let cfg = crate::helpers::configuration::Config::get();
        let err = crate::cluster::PipelineConfigView::for_name(&cfg, "missing-otel-pipeline")
            .expect_err("missing pipeline must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("missing-otel-pipeline") || msg.contains("not found"),
            "{msg}"
        );
    }

    #[test]
    fn cap_sets_truncated() {
        assert_eq!(cap_with_truncated(10, 100), (10, false));
        assert_eq!(cap_with_truncated(101, 100), (100, true));
        assert_eq!(cap_with_truncated(MAX_TRACES + 1, MAX_TRACES).1, true);
        assert_eq!(cap_with_truncated(MAX_LOG_ROWS, MAX_LOG_ROWS).1, false);
    }

    #[test]
    fn top_n_one_thousand_one_truncates() {
        let series: Vec<(String, f64)> =
            (0..1001).map(|i| (format!("s{i:04}"), i as f64)).collect();
        let (kept, truncated) = top_n_by_abs(series, MAX_SERIES);
        assert_eq!(kept.len(), MAX_SERIES);
        assert!(truncated);
        assert_eq!(kept[0].0, "s1000");
    }

    #[test]
    fn udfs_must_not_open_parquet_listing() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/sqlrt/udfs");
        fn visit_listing(path: &Path) {
            for entry in fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    visit_listing(&path);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                if path.file_name().and_then(|n| n.to_str()) == Some("mod.rs") {
                    continue;
                }
                let text = fs::read_to_string(&path).unwrap();
                assert!(
                    !text.contains("IcebergQueryUnavailable"),
                    "{} must not invent IcebergQueryUnavailable",
                    path.display()
                );
                assert!(
                    !text.contains("list_objects") && !text.contains("ListObjects"),
                    "{} must not list object storage",
                    path.display()
                );
            }
        }
        visit_listing(&root);
    }

    #[test]
    fn tui_lists_every_observability_udf() {
        let tui = include_str!("../tui.rs");
        assert!(
            tui.contains("observability_udf_names"),
            "tui.rs must autocomplete from observability_udf_names()"
        );
    }

    #[test]
    fn cookbook_names_match_registry() {
        let cookbook = include_str!("../../../examples/otel/cookbook.sql");
        for name in OBSERVABILITY_UDF_NAMES {
            assert!(cookbook.contains(name), "cookbook missing {name}");
        }
    }

    fn cookbook_statements() -> Vec<String> {
        include_str!("../../../examples/otel/cookbook.sql")
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.trim().starts_with("--"))
            .collect::<Vec<_>>()
            .join("\n")
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect()
    }

    #[tokio::test]
    async fn cookbook_runs_against_memtables() {
        use super::logs::logs_schema;
        use super::trace::spans_schema;
        use crate::sqlrt::session::build_query_context;
        use datafusion::arrow::array::{
            BooleanArray, Float64Array, Int64Array, ListBuilder, StringArray, UInt32Array,
            UInt64Builder,
        };
        use datafusion::arrow::datatypes::{DataType, Field, Schema};
        use datafusion::arrow::record_batch::RecordBatch;
        use datafusion::datasource::MemTable;
        use datafusion::prelude::SessionConfig;
        use std::sync::Arc;

        let ctx = build_query_context(SessionConfig::new());
        let spans = RecordBatch::try_new(
            spans_schema(),
            vec![
                Arc::new(StringArray::from(vec!["4bf92f3577b34da6a3ce929d0e0e4736"])),
                Arc::new(StringArray::from(vec!["s1"])),
                Arc::new(StringArray::from(vec![None::<&str>])),
                Arc::new(StringArray::from(vec!["root"])),
                Arc::new(StringArray::from(vec!["SPAN_KIND_SERVER"])),
                Arc::new(Int64Array::from(vec![1])),
                Arc::new(Int64Array::from(vec![2])),
                Arc::new(Int64Array::from(vec![1])),
                Arc::new(StringArray::from(vec!["STATUS_CODE_ERROR"])),
                Arc::new(StringArray::from(vec!["checkout"])),
                Arc::new(StringArray::from(vec![None::<&str>])),
                Arc::new(StringArray::from(vec!["t"])),
                Arc::new(StringArray::from(vec![None::<&str>])),
                Arc::new(UInt32Array::from(vec![0])),
                Arc::new(StringArray::from(vec!["{}"])),
                Arc::new(StringArray::from(vec!["{}"])),
                Arc::new(BooleanArray::from(vec![false])),
            ],
        )
        .unwrap();
        ctx.register_table(
            "spans",
            Arc::new(MemTable::try_new(spans_schema(), vec![vec![spans]]).unwrap()),
        )
        .unwrap();
        let logs = RecordBatch::try_new(
            logs_schema(),
            vec![
                Arc::new(Int64Array::from(vec![5])),
                Arc::new(Int64Array::from(vec![None])),
                Arc::new(StringArray::from(vec![Some("ERROR")])),
                Arc::new(Int64Array::from(vec![Some(17)])),
                Arc::new(StringArray::from(vec!["timeout"])),
                Arc::new(StringArray::from(vec![Some(
                    "4bf92f3577b34da6a3ce929d0e0e4736",
                )])),
                Arc::new(StringArray::from(vec![Some("s1")])),
                Arc::new(StringArray::from(vec!["checkout"])),
                Arc::new(StringArray::from(vec!["t"])),
                Arc::new(UInt32Array::from(vec![0])),
                Arc::new(StringArray::from(vec!["{}"])),
                Arc::new(StringArray::from(vec!["{}"])),
                Arc::new(BooleanArray::from(vec![false])),
            ],
        )
        .unwrap();
        ctx.register_table(
            "log_records",
            Arc::new(MemTable::try_new(logs_schema(), vec![vec![logs]]).unwrap()),
        )
        .unwrap();

        let sum_schema = Arc::new(Schema::new(vec![
            Field::new("service_name", DataType::Utf8, false),
            Field::new("value", DataType::Float64, false),
            Field::new("time_unix_nano", DataType::Int64, false),
            Field::new("metric_name", DataType::Utf8, false),
            Field::new("hour", DataType::UInt32, false),
        ]));
        let sum = RecordBatch::try_new(
            sum_schema.clone(),
            vec![
                Arc::new(StringArray::from(vec!["checkout", "checkout"])),
                Arc::new(Float64Array::from(vec![1.0, 2.0])),
                Arc::new(Int64Array::from(vec![0, 1_000_000_000])),
                Arc::new(StringArray::from(vec![
                    "http.server.duration",
                    "http.server.request.count",
                ])),
                Arc::new(UInt32Array::from(vec![0, 0])),
            ],
        )
        .unwrap();
        ctx.register_table(
            "sum",
            Arc::new(MemTable::try_new(sum_schema, vec![vec![sum]]).unwrap()),
        )
        .unwrap();

        let mut counts = ListBuilder::new(UInt64Builder::new());
        counts.values().append_value(1);
        counts.values().append_value(1);
        counts.append(true);
        let mut bounds = ListBuilder::new(datafusion::arrow::array::Float64Builder::new());
        bounds.values().append_value(1.0);
        bounds.append(true);
        let hist_schema = Arc::new(Schema::new(vec![
            Field::new("metric_name", DataType::Utf8, false),
            Field::new(
                "bucket_counts",
                DataType::List(Arc::new(Field::new("item", DataType::UInt64, true))),
                false,
            ),
            Field::new(
                "explicit_bounds",
                DataType::List(Arc::new(Field::new("item", DataType::Float64, true))),
                false,
            ),
        ]));
        let hist = RecordBatch::try_new(
            hist_schema.clone(),
            vec![
                Arc::new(StringArray::from(vec!["http.server.duration"])),
                Arc::new(counts.finish()),
                Arc::new(bounds.finish()),
            ],
        )
        .unwrap();
        ctx.register_table(
            "histogram",
            Arc::new(MemTable::try_new(hist_schema, vec![vec![hist]]).unwrap()),
        )
        .unwrap();

        for stmt in cookbook_statements() {
            ctx.sql(&stmt)
                .await
                .unwrap_or_else(|e| panic!("plan {stmt}: {e}"))
                .collect()
                .await
                .unwrap_or_else(|e| panic!("exec {stmt}: {e}"));
        }
    }

    #[tokio::test]
    async fn wide_raw_window_fails_closed_rollup_succeeds() {
        use crate::sqlrt::session::build_query_context;
        use datafusion::arrow::array::{Float64Array, StringArray};
        use datafusion::arrow::datatypes::{DataType, Field, Schema};
        use datafusion::arrow::record_batch::RecordBatch;
        use datafusion::datasource::MemTable;
        use datafusion::prelude::SessionConfig;
        use std::sync::Arc;

        assert!(matches!(
            ScanBudget::new_at(Some(0), Some(OTEL_DEFAULT_LOOKBACK + 1), None, 1).unwrap_err(),
            ScanBudgetError::RangeTooWide
        ));
        let ctx = build_query_context(SessionConfig::new());
        let schema = Arc::new(Schema::new(vec![
            Field::new("service_name", DataType::Utf8, false),
            Field::new("value", DataType::Float64, false),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(StringArray::from(vec!["checkout"])),
                Arc::new(Float64Array::from(vec![1.0])),
            ],
        )
        .unwrap();
        ctx.register_table(
            "sum_1m",
            Arc::new(MemTable::try_new(schema, vec![vec![batch]]).unwrap()),
        )
        .unwrap();
        let rows = ctx
            .sql("SELECT service_name, value FROM sum_1m")
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert_eq!(rows[0].num_rows(), 1);
    }
}
