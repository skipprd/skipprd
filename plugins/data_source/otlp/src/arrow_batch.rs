use std::sync::Arc;

use arrow::array::{
    ArrayRef, BooleanArray, Float64Array, Int32Array, Int64Array, ListArray, RecordBatch,
    StringArray, UInt32Array,
};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use skippr_runtime_sdk::helpers::offsets::OffsetKey;
use skippr_runtime_sdk::protocol::{RuntimeIngestPartitionBatch, RuntimeOffsetPosition};
use skippr_runtime_sdk::sdk::encode_record_batches;

use crate::bronze::{
    ExponentialHistogramRecord, GaugeRecord, HistogramRecord, LogRecord, SpanEventRecord,
    SpanLinkRecord, SpanRecord, SumRecord,
};
use crate::decode::DecodedSignal;

pub const NS_SPANS: &str = "spans";
pub const NS_SPAN_EVENTS: &str = "span_events";
pub const NS_SPAN_LINKS: &str = "span_links";
pub const NS_LOG_RECORDS: &str = "log_records";
pub const NS_GAUGE: &str = "gauge";
pub const NS_SUM: &str = "sum";
pub const NS_HISTOGRAM: &str = "histogram";
pub const NS_EXP_HISTOGRAM: &str = "exponential_histogram";

#[derive(Debug)]
pub enum ArrowBuildError {
    Schema(String),
}

impl std::fmt::Display for ArrowBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Schema(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for ArrowBuildError {}

fn map_json<K: serde::Serialize>(map: &K) -> String {
    serde_json::to_string(map).unwrap_or_else(|_| "{}".to_string())
}

fn list_u64(values: &[Vec<u64>]) -> ListArray {
    let field = Arc::new(Field::new("item", DataType::UInt64, true));
    let mut builder =
        arrow::array::ListBuilder::new(arrow::array::UInt64Builder::new()).with_field(field);
    for row in values {
        for v in row {
            builder.values().append_value(*v);
        }
        builder.append(true);
    }
    builder.finish()
}

fn list_f64(values: &[Vec<f64>]) -> ListArray {
    let field = Arc::new(Field::new("item", DataType::Float64, true));
    let mut builder =
        arrow::array::ListBuilder::new(arrow::array::Float64Builder::new()).with_field(field);
    for row in values {
        for v in row {
            builder.values().append_value(*v);
        }
        builder.append(true);
    }
    builder.finish()
}

fn utf8(values: Vec<Option<String>>) -> ArrayRef {
    Arc::new(StringArray::from(values))
}

fn utf8_req(values: Vec<String>) -> ArrayRef {
    Arc::new(StringArray::from(values))
}

pub fn spans_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("trace_id", DataType::Utf8, false),
        Field::new("span_id", DataType::Utf8, false),
        Field::new("parent_span_id", DataType::Utf8, true),
        Field::new("name", DataType::Utf8, false),
        Field::new("kind", DataType::Utf8, false),
        Field::new("start_time_unix_nano", DataType::Int64, false),
        Field::new("end_time_unix_nano", DataType::Int64, false),
        Field::new("duration_nano", DataType::Int64, false),
        Field::new("status_code", DataType::Utf8, false),
        Field::new("service_name", DataType::Utf8, false),
        Field::new("deployment_environment", DataType::Utf8, true),
        Field::new("tenant_id", DataType::Utf8, false),
        Field::new("http_route", DataType::Utf8, true),
        Field::new("hour", DataType::UInt32, false),
        Field::new("resource_attributes", DataType::Utf8, false),
        Field::new("span_attributes", DataType::Utf8, false),
        Field::new("dropped_attributes_count", DataType::Int64, false),
        Field::new("dropped_events_count", DataType::Int64, false),
        Field::new("dropped_links_count", DataType::Int64, false),
        Field::new("scope_name", DataType::Utf8, true),
        Field::new("scope_version", DataType::Utf8, true),
    ]))
}

pub fn spans_batch(rows: &[SpanRecord]) -> Result<RecordBatch, ArrowBuildError> {
    RecordBatch::try_new(
        spans_schema(),
        vec![
            utf8_req(rows.iter().map(|r| r.trace_id.to_hex()).collect()),
            utf8_req(rows.iter().map(|r| r.span_id.to_hex()).collect()),
            utf8(
                rows.iter()
                    .map(|r| r.parent_span_id.map(|id| id.to_hex()))
                    .collect(),
            ),
            utf8_req(rows.iter().map(|r| r.name.clone()).collect()),
            utf8_req(
                rows.iter()
                    .map(|r| format!("{:?}", r.kind).to_uppercase())
                    .collect(),
            ),
            Arc::new(Int64Array::from(
                rows.iter()
                    .map(|r| r.start_time_unix_nano as i64)
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Int64Array::from(
                rows.iter()
                    .map(|r| r.end_time_unix_nano as i64)
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Int64Array::from(
                rows.iter()
                    .map(|r| r.duration_nano as i64)
                    .collect::<Vec<_>>(),
            )),
            utf8_req(
                rows.iter()
                    .map(|r| format!("STATUS_CODE_{:?}", r.status_code).to_uppercase())
                    .collect(),
            ),
            utf8_req(rows.iter().map(|r| r.service_name.clone()).collect()),
            utf8(
                rows.iter()
                    .map(|r| r.deployment_environment.clone())
                    .collect(),
            ),
            utf8_req(rows.iter().map(|r| r.tenant_id.clone()).collect()),
            utf8(rows.iter().map(|r| r.http_route.clone()).collect()),
            Arc::new(UInt32Array::from(
                rows.iter().map(|r| r.hour).collect::<Vec<_>>(),
            )),
            utf8_req(
                rows.iter()
                    .map(|r| map_json(&r.resource_attributes))
                    .collect(),
            ),
            utf8_req(rows.iter().map(|r| map_json(&r.span_attributes)).collect()),
            Arc::new(Int64Array::from(
                rows.iter()
                    .map(|r| i64::from(r.dropped_attributes_count))
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Int64Array::from(
                rows.iter()
                    .map(|r| i64::from(r.dropped_events_count))
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Int64Array::from(
                rows.iter()
                    .map(|r| i64::from(r.dropped_links_count))
                    .collect::<Vec<_>>(),
            )),
            utf8(rows.iter().map(|r| r.scope_name.clone()).collect()),
            utf8(rows.iter().map(|r| r.scope_version.clone()).collect()),
        ],
    )
    .map_err(|e| ArrowBuildError::Schema(e.to_string()))
}

pub fn span_events_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("trace_id", DataType::Utf8, false),
        Field::new("span_id", DataType::Utf8, false),
        Field::new("time_unix_nano", DataType::Int64, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("service_name", DataType::Utf8, false),
        Field::new("tenant_id", DataType::Utf8, false),
        Field::new("hour", DataType::UInt32, false),
        Field::new("attributes", DataType::Utf8, false),
        Field::new("dropped_attributes_count", DataType::Int64, false),
    ]))
}

pub fn span_events_batch(rows: &[SpanEventRecord]) -> Result<RecordBatch, ArrowBuildError> {
    RecordBatch::try_new(
        span_events_schema(),
        vec![
            utf8_req(rows.iter().map(|r| r.trace_id.to_hex()).collect()),
            utf8_req(rows.iter().map(|r| r.span_id.to_hex()).collect()),
            Arc::new(Int64Array::from(
                rows.iter()
                    .map(|r| r.time_unix_nano as i64)
                    .collect::<Vec<_>>(),
            )),
            utf8_req(rows.iter().map(|r| r.name.clone()).collect()),
            utf8_req(rows.iter().map(|r| r.service_name.clone()).collect()),
            utf8_req(rows.iter().map(|r| r.tenant_id.clone()).collect()),
            Arc::new(UInt32Array::from(
                rows.iter().map(|r| r.hour).collect::<Vec<_>>(),
            )),
            utf8_req(rows.iter().map(|r| map_json(&r.attributes)).collect()),
            Arc::new(Int64Array::from(
                rows.iter()
                    .map(|r| i64::from(r.dropped_attributes_count))
                    .collect::<Vec<_>>(),
            )),
        ],
    )
    .map_err(|e| ArrowBuildError::Schema(e.to_string()))
}

pub fn span_links_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("trace_id", DataType::Utf8, false),
        Field::new("span_id", DataType::Utf8, false),
        Field::new("linked_trace_id", DataType::Utf8, false),
        Field::new("linked_span_id", DataType::Utf8, false),
        Field::new("service_name", DataType::Utf8, false),
        Field::new("tenant_id", DataType::Utf8, false),
        Field::new("hour", DataType::UInt32, false),
        Field::new("attributes", DataType::Utf8, false),
        Field::new("dropped_attributes_count", DataType::Int64, false),
    ]))
}

pub fn span_links_batch(rows: &[SpanLinkRecord]) -> Result<RecordBatch, ArrowBuildError> {
    RecordBatch::try_new(
        span_links_schema(),
        vec![
            utf8_req(rows.iter().map(|r| r.trace_id.to_hex()).collect()),
            utf8_req(rows.iter().map(|r| r.span_id.to_hex()).collect()),
            utf8_req(rows.iter().map(|r| r.linked_trace_id.to_hex()).collect()),
            utf8_req(rows.iter().map(|r| r.linked_span_id.to_hex()).collect()),
            utf8_req(rows.iter().map(|r| r.service_name.clone()).collect()),
            utf8_req(rows.iter().map(|r| r.tenant_id.clone()).collect()),
            Arc::new(UInt32Array::from(
                rows.iter().map(|r| r.hour).collect::<Vec<_>>(),
            )),
            utf8_req(rows.iter().map(|r| map_json(&r.attributes)).collect()),
            Arc::new(Int64Array::from(
                rows.iter()
                    .map(|r| i64::from(r.dropped_attributes_count))
                    .collect::<Vec<_>>(),
            )),
        ],
    )
    .map_err(|e| ArrowBuildError::Schema(e.to_string()))
}

pub fn logs_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("time_unix_nano", DataType::Int64, false),
        Field::new("observed_time_unix_nano", DataType::Int64, true),
        Field::new("severity_text", DataType::Utf8, true),
        Field::new("severity_number", DataType::Int64, true),
        Field::new("body", DataType::Utf8, false),
        Field::new("trace_id", DataType::Utf8, true),
        Field::new("span_id", DataType::Utf8, true),
        Field::new("service_name", DataType::Utf8, false),
        Field::new("tenant_id", DataType::Utf8, false),
        Field::new("hour", DataType::UInt32, false),
        Field::new("resource_attributes", DataType::Utf8, false),
        Field::new("log_attributes", DataType::Utf8, false),
        Field::new("dropped_attributes_count", DataType::Int64, false),
        Field::new("scope_name", DataType::Utf8, true),
    ]))
}

pub fn logs_batch(rows: &[LogRecord]) -> Result<RecordBatch, ArrowBuildError> {
    RecordBatch::try_new(
        logs_schema(),
        vec![
            Arc::new(Int64Array::from(
                rows.iter()
                    .map(|r| r.time_unix_nano as i64)
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Int64Array::from(
                rows.iter()
                    .map(|r| r.observed_time_unix_nano.map(|v| v as i64))
                    .collect::<Vec<_>>(),
            )),
            utf8(rows.iter().map(|r| r.severity_text.clone()).collect()),
            Arc::new(Int64Array::from(
                rows.iter()
                    .map(|r| r.severity_number.map(i64::from))
                    .collect::<Vec<_>>(),
            )),
            utf8_req(rows.iter().map(|r| r.body.clone()).collect()),
            utf8(
                rows.iter()
                    .map(|r| r.trace_id.map(|id| id.to_hex()))
                    .collect(),
            ),
            utf8(
                rows.iter()
                    .map(|r| r.span_id.map(|id| id.to_hex()))
                    .collect(),
            ),
            utf8_req(rows.iter().map(|r| r.service_name.clone()).collect()),
            utf8_req(rows.iter().map(|r| r.tenant_id.clone()).collect()),
            Arc::new(UInt32Array::from(
                rows.iter().map(|r| r.hour).collect::<Vec<_>>(),
            )),
            utf8_req(
                rows.iter()
                    .map(|r| map_json(&r.resource_attributes))
                    .collect(),
            ),
            utf8_req(rows.iter().map(|r| map_json(&r.log_attributes)).collect()),
            Arc::new(Int64Array::from(
                rows.iter()
                    .map(|r| i64::from(r.dropped_attributes_count))
                    .collect::<Vec<_>>(),
            )),
            utf8(rows.iter().map(|r| r.scope_name.clone()).collect()),
        ],
    )
    .map_err(|e| ArrowBuildError::Schema(e.to_string()))
}

fn metric_common_fields() -> Vec<Field> {
    vec![
        Field::new("metric_name", DataType::Utf8, false),
        Field::new("unit", DataType::Utf8, false),
        Field::new("time_unix_nano", DataType::Int64, false),
        Field::new("start_time_unix_nano", DataType::Int64, true),
    ]
}

pub fn gauge_schema() -> SchemaRef {
    let mut fields = metric_common_fields();
    fields.extend([
        Field::new("value", DataType::Float64, false),
        Field::new("service_name", DataType::Utf8, false),
        Field::new("tenant_id", DataType::Utf8, false),
        Field::new("hour", DataType::UInt32, false),
        Field::new("resource_attributes", DataType::Utf8, false),
        Field::new("metric_attributes", DataType::Utf8, false),
        Field::new("exemplar_trace_id", DataType::Utf8, true),
        Field::new("scope_name", DataType::Utf8, true),
    ]);
    Arc::new(Schema::new(fields))
}

pub fn gauge_batch(rows: &[GaugeRecord]) -> Result<RecordBatch, ArrowBuildError> {
    RecordBatch::try_new(
        gauge_schema(),
        vec![
            utf8_req(rows.iter().map(|r| r.metric_name.clone()).collect()),
            utf8_req(rows.iter().map(|r| r.unit.clone()).collect()),
            Arc::new(Int64Array::from(
                rows.iter()
                    .map(|r| r.time_unix_nano as i64)
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Int64Array::from(
                rows.iter()
                    .map(|r| r.start_time_unix_nano.map(|v| v as i64))
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                rows.iter().map(|r| r.value).collect::<Vec<_>>(),
            )),
            utf8_req(rows.iter().map(|r| r.service_name.clone()).collect()),
            utf8_req(rows.iter().map(|r| r.tenant_id.clone()).collect()),
            Arc::new(UInt32Array::from(
                rows.iter().map(|r| r.hour).collect::<Vec<_>>(),
            )),
            utf8_req(
                rows.iter()
                    .map(|r| map_json(&r.resource_attributes))
                    .collect(),
            ),
            utf8_req(
                rows.iter()
                    .map(|r| map_json(&r.metric_attributes))
                    .collect(),
            ),
            utf8(
                rows.iter()
                    .map(|r| r.exemplar_trace_id.map(|id| id.to_hex()))
                    .collect(),
            ),
            utf8(rows.iter().map(|r| r.scope_name.clone()).collect()),
        ],
    )
    .map_err(|e| ArrowBuildError::Schema(e.to_string()))
}

pub fn sum_schema() -> SchemaRef {
    let mut fields = metric_common_fields();
    fields.extend([
        Field::new("value", DataType::Float64, false),
        Field::new("is_monotonic", DataType::Boolean, false),
        Field::new("aggregation_temporality", DataType::Utf8, false),
        Field::new("service_name", DataType::Utf8, false),
        Field::new("tenant_id", DataType::Utf8, false),
        Field::new("hour", DataType::UInt32, false),
        Field::new("resource_attributes", DataType::Utf8, false),
        Field::new("metric_attributes", DataType::Utf8, false),
        Field::new("exemplar_trace_id", DataType::Utf8, true),
        Field::new("scope_name", DataType::Utf8, true),
    ]);
    Arc::new(Schema::new(fields))
}

pub fn sum_batch(rows: &[SumRecord]) -> Result<RecordBatch, ArrowBuildError> {
    RecordBatch::try_new(
        sum_schema(),
        vec![
            utf8_req(rows.iter().map(|r| r.metric_name.clone()).collect()),
            utf8_req(rows.iter().map(|r| r.unit.clone()).collect()),
            Arc::new(Int64Array::from(
                rows.iter()
                    .map(|r| r.time_unix_nano as i64)
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Int64Array::from(
                rows.iter()
                    .map(|r| r.start_time_unix_nano.map(|v| v as i64))
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                rows.iter().map(|r| r.value).collect::<Vec<_>>(),
            )),
            Arc::new(BooleanArray::from(
                rows.iter().map(|r| r.is_monotonic).collect::<Vec<_>>(),
            )),
            utf8_req(
                rows.iter()
                    .map(|r| format!("{:?}", r.aggregation_temporality).to_uppercase())
                    .collect(),
            ),
            utf8_req(rows.iter().map(|r| r.service_name.clone()).collect()),
            utf8_req(rows.iter().map(|r| r.tenant_id.clone()).collect()),
            Arc::new(UInt32Array::from(
                rows.iter().map(|r| r.hour).collect::<Vec<_>>(),
            )),
            utf8_req(
                rows.iter()
                    .map(|r| map_json(&r.resource_attributes))
                    .collect(),
            ),
            utf8_req(
                rows.iter()
                    .map(|r| map_json(&r.metric_attributes))
                    .collect(),
            ),
            utf8(
                rows.iter()
                    .map(|r| r.exemplar_trace_id.map(|id| id.to_hex()))
                    .collect(),
            ),
            utf8(rows.iter().map(|r| r.scope_name.clone()).collect()),
        ],
    )
    .map_err(|e| ArrowBuildError::Schema(e.to_string()))
}

pub fn histogram_schema() -> SchemaRef {
    let mut fields = metric_common_fields();
    fields.extend([
        Field::new("count", DataType::Int64, false),
        Field::new("sum", DataType::Float64, true),
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
        Field::new("service_name", DataType::Utf8, false),
        Field::new("tenant_id", DataType::Utf8, false),
        Field::new("hour", DataType::UInt32, false),
        Field::new("resource_attributes", DataType::Utf8, false),
        Field::new("metric_attributes", DataType::Utf8, false),
        Field::new("exemplar_trace_id", DataType::Utf8, true),
        Field::new("scope_name", DataType::Utf8, true),
    ]);
    Arc::new(Schema::new(fields))
}

pub fn histogram_batch(rows: &[HistogramRecord]) -> Result<RecordBatch, ArrowBuildError> {
    RecordBatch::try_new(
        histogram_schema(),
        vec![
            utf8_req(rows.iter().map(|r| r.metric_name.clone()).collect()),
            utf8_req(rows.iter().map(|r| r.unit.clone()).collect()),
            Arc::new(Int64Array::from(
                rows.iter()
                    .map(|r| r.time_unix_nano as i64)
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Int64Array::from(
                rows.iter()
                    .map(|r| r.start_time_unix_nano.map(|v| v as i64))
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Int64Array::from(
                rows.iter().map(|r| r.count as i64).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                rows.iter().map(|r| r.sum).collect::<Vec<_>>(),
            )),
            Arc::new(list_u64(
                &rows
                    .iter()
                    .map(|r| r.bucket_counts.clone())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(list_f64(
                &rows
                    .iter()
                    .map(|r| r.explicit_bounds.clone())
                    .collect::<Vec<_>>(),
            )),
            utf8_req(rows.iter().map(|r| r.service_name.clone()).collect()),
            utf8_req(rows.iter().map(|r| r.tenant_id.clone()).collect()),
            Arc::new(UInt32Array::from(
                rows.iter().map(|r| r.hour).collect::<Vec<_>>(),
            )),
            utf8_req(
                rows.iter()
                    .map(|r| map_json(&r.resource_attributes))
                    .collect(),
            ),
            utf8_req(
                rows.iter()
                    .map(|r| map_json(&r.metric_attributes))
                    .collect(),
            ),
            utf8(
                rows.iter()
                    .map(|r| r.exemplar_trace_id.map(|id| id.to_hex()))
                    .collect(),
            ),
            utf8(rows.iter().map(|r| r.scope_name.clone()).collect()),
        ],
    )
    .map_err(|e| ArrowBuildError::Schema(e.to_string()))
}

pub fn exponential_histogram_schema() -> SchemaRef {
    let mut fields = metric_common_fields();
    fields.extend([
        Field::new("count", DataType::Int64, false),
        Field::new("sum", DataType::Float64, true),
        Field::new("scale", DataType::Int32, false),
        Field::new("zero_count", DataType::Int64, false),
        Field::new("positive_offset", DataType::Int32, false),
        Field::new(
            "positive_bucket_counts",
            DataType::List(Arc::new(Field::new("item", DataType::UInt64, true))),
            false,
        ),
        Field::new("negative_offset", DataType::Int32, false),
        Field::new(
            "negative_bucket_counts",
            DataType::List(Arc::new(Field::new("item", DataType::UInt64, true))),
            false,
        ),
        Field::new("service_name", DataType::Utf8, false),
        Field::new("tenant_id", DataType::Utf8, false),
        Field::new("hour", DataType::UInt32, false),
        Field::new("resource_attributes", DataType::Utf8, false),
        Field::new("metric_attributes", DataType::Utf8, false),
        Field::new("exemplar_trace_id", DataType::Utf8, true),
        Field::new("scope_name", DataType::Utf8, true),
    ]);
    Arc::new(Schema::new(fields))
}

pub fn exponential_histogram_batch(
    rows: &[ExponentialHistogramRecord],
) -> Result<RecordBatch, ArrowBuildError> {
    RecordBatch::try_new(
        exponential_histogram_schema(),
        vec![
            utf8_req(rows.iter().map(|r| r.metric_name.clone()).collect()),
            utf8_req(rows.iter().map(|r| r.unit.clone()).collect()),
            Arc::new(Int64Array::from(
                rows.iter()
                    .map(|r| r.time_unix_nano as i64)
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Int64Array::from(
                rows.iter()
                    .map(|r| r.start_time_unix_nano.map(|v| v as i64))
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Int64Array::from(
                rows.iter().map(|r| r.count as i64).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                rows.iter().map(|r| r.sum).collect::<Vec<_>>(),
            )),
            Arc::new(Int32Array::from(
                rows.iter().map(|r| r.scale).collect::<Vec<_>>(),
            )),
            Arc::new(Int64Array::from(
                rows.iter().map(|r| r.zero_count as i64).collect::<Vec<_>>(),
            )),
            Arc::new(Int32Array::from(
                rows.iter().map(|r| r.positive_offset).collect::<Vec<_>>(),
            )),
            Arc::new(list_u64(
                &rows
                    .iter()
                    .map(|r| r.positive_bucket_counts.clone())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Int32Array::from(
                rows.iter().map(|r| r.negative_offset).collect::<Vec<_>>(),
            )),
            Arc::new(list_u64(
                &rows
                    .iter()
                    .map(|r| r.negative_bucket_counts.clone())
                    .collect::<Vec<_>>(),
            )),
            utf8_req(rows.iter().map(|r| r.service_name.clone()).collect()),
            utf8_req(rows.iter().map(|r| r.tenant_id.clone()).collect()),
            Arc::new(UInt32Array::from(
                rows.iter().map(|r| r.hour).collect::<Vec<_>>(),
            )),
            utf8_req(
                rows.iter()
                    .map(|r| map_json(&r.resource_attributes))
                    .collect(),
            ),
            utf8_req(
                rows.iter()
                    .map(|r| map_json(&r.metric_attributes))
                    .collect(),
            ),
            utf8(
                rows.iter()
                    .map(|r| r.exemplar_trace_id.map(|id| id.to_hex()))
                    .collect(),
            ),
            utf8(rows.iter().map(|r| r.scope_name.clone()).collect()),
        ],
    )
    .map_err(|e| ArrowBuildError::Schema(e.to_string()))
}

pub fn deadletter_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("namespace", DataType::Utf8, false),
        Field::new("record", DataType::Utf8, false),
        Field::new("error", DataType::Utf8, false),
        Field::new("failure_code", DataType::Utf8, false),
        Field::new("event_time", DataType::Int64, true),
        Field::new("processed_time", DataType::Int64, false),
        Field::new("source_uri", DataType::Utf8, true),
        Field::new("offset_key", DataType::Utf8, true),
        Field::new("offset_pos", DataType::Int64, true),
    ]))
}

pub fn deadletter_batch(
    namespace: &str,
    error: &str,
    failure_code: &str,
    source_uri: &str,
    offset_key: &str,
    offset_pos: i64,
) -> Result<RecordBatch, ArrowBuildError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    RecordBatch::try_new(
        deadletter_schema(),
        vec![
            utf8_req(vec![format!("dl-{offset_pos}")]),
            utf8_req(vec![namespace.to_string()]),
            utf8_req(vec![String::new()]),
            utf8_req(vec![error.to_string()]),
            utf8_req(vec![failure_code.to_string()]),
            Arc::new(Int64Array::from(vec![None])),
            Arc::new(Int64Array::from(vec![now])),
            utf8(vec![Some(source_uri.to_string())]),
            utf8(vec![Some(offset_key.to_string())]),
            Arc::new(Int64Array::from(vec![Some(offset_pos)])),
        ],
    )
    .map_err(|e| ArrowBuildError::Schema(e.to_string()))
}

pub fn ipc_batch(
    namespace: &str,
    partition: &str,
    batch: RecordBatch,
    offset_pos: u64,
) -> Result<RuntimeIngestPartitionBatch, ArrowBuildError> {
    let bytes =
        encode_record_batches(&[batch]).map_err(|e| ArrowBuildError::Schema(e.to_string()))?;
    Ok(RuntimeIngestPartitionBatch {
        sink_ref: namespace.to_string(),
        namespace: namespace.to_string(),
        partition: partition.to_string(),
        time: None,
        schema_fingerprint: String::new(),
        offsets: vec![RuntimeOffsetPosition {
            key: OffsetKey::new(namespace, partition),
            position: offset_pos,
        }],
        arrow_stream_bytes: bytes,
        cdc_rows: None,
        checkpoint_update: None,
    })
}

pub fn batches_from_signal(
    signal: &DecodedSignal,
    offset_pos: u64,
) -> Result<Vec<RuntimeIngestPartitionBatch>, ArrowBuildError> {
    let mut out = Vec::new();
    match signal {
        DecodedSignal::Traces(t) => {
            if !t.spans.is_empty() {
                out.push(ipc_batch(
                    NS_SPANS,
                    &offset_pos.to_string(),
                    spans_batch(&t.spans)?,
                    offset_pos,
                )?);
            }
            if !t.events.is_empty() {
                out.push(ipc_batch(
                    NS_SPAN_EVENTS,
                    &offset_pos.to_string(),
                    span_events_batch(&t.events)?,
                    offset_pos,
                )?);
            }
            if !t.links.is_empty() {
                out.push(ipc_batch(
                    NS_SPAN_LINKS,
                    &offset_pos.to_string(),
                    span_links_batch(&t.links)?,
                    offset_pos,
                )?);
            }
        }
        DecodedSignal::Logs(l) => {
            if !l.records.is_empty() {
                out.push(ipc_batch(
                    NS_LOG_RECORDS,
                    &offset_pos.to_string(),
                    logs_batch(&l.records)?,
                    offset_pos,
                )?);
            }
        }
        DecodedSignal::Metrics(m) => {
            if !m.gauges.is_empty() {
                out.push(ipc_batch(
                    NS_GAUGE,
                    &offset_pos.to_string(),
                    gauge_batch(&m.gauges)?,
                    offset_pos,
                )?);
            }
            if !m.sums.is_empty() {
                out.push(ipc_batch(
                    NS_SUM,
                    &offset_pos.to_string(),
                    sum_batch(&m.sums)?,
                    offset_pos,
                )?);
            }
            if !m.histograms.is_empty() {
                out.push(ipc_batch(
                    NS_HISTOGRAM,
                    &offset_pos.to_string(),
                    histogram_batch(&m.histograms)?,
                    offset_pos,
                )?);
            }
            if !m.exponential_histograms.is_empty() {
                out.push(ipc_batch(
                    NS_EXP_HISTOGRAM,
                    &offset_pos.to_string(),
                    exponential_histogram_batch(&m.exponential_histograms)?,
                    offset_pos,
                )?);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bronze::{SpanId, SpanKind, SpanRecord, StatusCode, TraceId};
    use std::collections::BTreeMap;

    #[test]
    fn span_column_names_match_seed() {
        let schema = spans_schema();
        let names: Vec<_> = schema.fields().iter().map(|f| f.name().as_str()).collect();
        for required in [
            "trace_id",
            "span_id",
            "parent_span_id",
            "name",
            "kind",
            "start_time_unix_nano",
            "end_time_unix_nano",
            "duration_nano",
            "status_code",
            "service_name",
            "tenant_id",
            "hour",
        ] {
            assert!(names.contains(&required), "missing {required}");
        }
    }

    #[test]
    fn exponential_histogram_has_scale() {
        let schema = exponential_histogram_schema();
        let names: Vec<_> = schema.fields().iter().map(|f| f.name().as_str()).collect();
        for required in [
            "scale",
            "zero_count",
            "positive_bucket_counts",
            "hour",
            "tenant_id",
        ] {
            assert!(names.contains(&required), "missing {required}");
        }
        let seed = include_str!("../../../../examples/otel/otel_columns.txt");
        assert!(seed.contains("[exponential_histogram]"));
        assert!(seed.contains("scale"));
    }

    #[test]
    fn ipc_round_trip() {
        let rec = SpanRecord {
            trace_id: TraceId::from_hex("4bf92f3577b34da6a3ce929d0e0e4736").unwrap(),
            span_id: SpanId::from_hex("00f067aa0ba902b7").unwrap(),
            parent_span_id: None,
            name: "root".into(),
            kind: SpanKind::Server,
            start_time_unix_nano: 1,
            end_time_unix_nano: 2,
            duration_nano: 1,
            status_code: StatusCode::Ok,
            service_name: "checkout".into(),
            deployment_environment: None,
            tenant_id: "acme".into(),
            http_route: None,
            hour: 0,
            resource_attributes: BTreeMap::new(),
            span_attributes: BTreeMap::new(),
            dropped_attributes_count: 0,
            dropped_events_count: 0,
            dropped_links_count: 0,
            scope_name: None,
            scope_version: None,
        };
        let batch = spans_batch(&[rec]).unwrap();
        let ipc = encode_record_batches(&[batch]).unwrap();
        assert!(!ipc.is_empty());
    }
}
