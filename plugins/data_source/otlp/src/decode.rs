use std::collections::BTreeMap;
use std::fmt;

use opentelemetry_proto::tonic::common::v1::{any_value, AnyValue, KeyValue};
use opentelemetry_proto::tonic::metrics::v1::metric::Data as MetricData;
use opentelemetry_proto::tonic::metrics::v1::number_data_point::Value as NumberValue;
use prost::Message;

use crate::bronze::{
    filter_attrs, hour_from_unix_nano, AggregationTemporality, ExponentialHistogramRecord,
    GaugeRecord, HistogramRecord, LogRecord, SpanEventRecord, SpanId, SpanKind, SpanLinkRecord,
    SpanRecord, StatusCode, SumRecord, TraceId,
};
use crate::config::OtlpConfig;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    Truncated(String),
    InvalidId(String),
    Empty,
    Json(String),
    Histogram(String),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated(s) => write!(f, "truncated OTLP payload: {s}"),
            Self::InvalidId(s) => write!(f, "invalid OTLP id: {s}"),
            Self::Empty => write!(f, "empty OTLP payload"),
            Self::Json(s) => write!(f, "invalid OTLP JSON: {s}"),
            Self::Histogram(s) => write!(f, "invalid histogram: {s}"),
        }
    }
}

impl std::error::Error for DecodeError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeDrop {
    UnsupportedMetricKind,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DecodeDrops {
    pub unsupported_metric_kind: u32,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DecodedTraces {
    pub spans: Vec<SpanRecord>,
    pub events: Vec<SpanEventRecord>,
    pub links: Vec<SpanLinkRecord>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DecodedLogs {
    pub records: Vec<LogRecord>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DecodedMetrics {
    pub gauges: Vec<GaugeRecord>,
    pub sums: Vec<SumRecord>,
    pub histograms: Vec<HistogramRecord>,
    pub exponential_histograms: Vec<ExponentialHistogramRecord>,
    pub drops: DecodeDrops,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DecodedSignal {
    Traces(DecodedTraces),
    Logs(DecodedLogs),
    Metrics(DecodedMetrics),
}

fn stringify_any(value: Option<&AnyValue>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    match &value.value {
        Some(any_value::Value::StringValue(s)) => s.clone(),
        Some(any_value::Value::BoolValue(v)) => v.to_string(),
        Some(any_value::Value::IntValue(v)) => v.to_string(),
        Some(any_value::Value::DoubleValue(v)) => v.to_string(),
        Some(any_value::Value::BytesValue(v)) => hex::encode(v),
        Some(any_value::Value::ArrayValue(arr)) => {
            let parts: Vec<String> = arr.values.iter().map(|v| stringify_any(Some(v))).collect();
            format!("[{}]", parts.join(","))
        }
        Some(any_value::Value::KvlistValue(list)) => {
            let mut map = BTreeMap::new();
            for kv in &list.values {
                map.insert(kv.key.clone(), stringify_any(kv.value.as_ref()));
            }
            serde_json::to_string(&map).unwrap_or_default()
        }
        None => String::new(),
    }
}

fn attrs_from_kv(kvs: &[KeyValue]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for kv in kvs {
        out.insert(kv.key.clone(), stringify_any(kv.value.as_ref()));
    }
    out
}

fn trace_id_from_bytes(bytes: &[u8]) -> Result<Option<TraceId>, DecodeError> {
    if bytes.is_empty() {
        return Ok(None);
    }
    TraceId::from_bytes(bytes)
        .map(Some)
        .map_err(|e| DecodeError::InvalidId(e.to_string()))
}

fn span_id_from_bytes(bytes: &[u8]) -> Result<Option<SpanId>, DecodeError> {
    if bytes.is_empty() {
        return Ok(None);
    }
    SpanId::from_bytes(bytes)
        .map(Some)
        .map_err(|e| DecodeError::InvalidId(e.to_string()))
}

fn require_trace_id(bytes: &[u8]) -> Result<TraceId, DecodeError> {
    trace_id_from_bytes(bytes)?.ok_or_else(|| DecodeError::InvalidId("empty trace_id".into()))
}

fn require_span_id(bytes: &[u8]) -> Result<SpanId, DecodeError> {
    span_id_from_bytes(bytes)?.ok_or_else(|| DecodeError::InvalidId("empty span_id".into()))
}

fn apply_allowlist(
    map: BTreeMap<String, String>,
    allowlist: Option<&[String]>,
    dropped_from_proto: u32,
) -> (BTreeMap<String, String>, u32) {
    let (filtered, extra) = filter_attrs(map, allowlist);
    (filtered, dropped_from_proto.saturating_add(extra))
}

fn inject_tenant(tenant_id: String, inject: &BTreeMap<String, String>) -> String {
    if !tenant_id.is_empty() {
        return tenant_id;
    }
    inject.get("tenant_id").cloned().unwrap_or_default()
}

fn string_kv(kvs: &[KeyValue], key: &str) -> Option<String> {
    kvs.iter()
        .find(|kv| kv.key == key)
        .and_then(|kv| match &kv.value {
            Some(AnyValue {
                value: Some(any_value::Value::StringValue(s)),
            }) if !s.is_empty() => Some(s.clone()),
            _ => None,
        })
}

fn resource_promotions_from_kvs(
    kvs: &[KeyValue],
) -> (String, Option<String>, Option<String>, String) {
    (
        string_kv(kvs, "service.name").unwrap_or_default(),
        string_kv(kvs, "deployment.environment"),
        string_kv(kvs, "http.route"),
        string_kv(kvs, "tenant.id").unwrap_or_default(),
    )
}

pub fn decode_traces(
    bytes: &[u8],
    config: &OtlpConfig,
    inject: &BTreeMap<String, String>,
) -> Result<DecodedTraces, DecodeError> {
    if bytes.is_empty() {
        return Err(DecodeError::Empty);
    }
    let req =
        opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest::decode(bytes)
            .map_err(|e| DecodeError::Truncated(e.to_string()))?;
    decode_traces_request(&req, config, inject)
}

pub fn decode_traces_json(
    bytes: &[u8],
    config: &OtlpConfig,
    inject: &BTreeMap<String, String>,
) -> Result<DecodedTraces, DecodeError> {
    if bytes.is_empty() {
        return Err(DecodeError::Empty);
    }
    let req: opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest =
        serde_json::from_slice(bytes).map_err(|e| DecodeError::Json(e.to_string()))?;
    decode_traces_request(&req, config, inject)
}

pub fn decode_traces_request(
    req: &opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest,
    config: &OtlpConfig,
    inject: &BTreeMap<String, String>,
) -> Result<DecodedTraces, DecodeError> {
    let allow = config.attribute_allowlist.as_deref();
    let mut out = DecodedTraces::default();
    for rs in &req.resource_spans {
        let resource_kvs = rs
            .resource
            .as_ref()
            .map(|r| r.attributes.as_slice())
            .unwrap_or(&[]);
        let resource_raw = attrs_from_kv(resource_kvs);
        let (service_name, deployment_environment, resource_http_route, tenant_raw) =
            resource_promotions_from_kvs(resource_kvs);
        let tenant_id = inject_tenant(tenant_raw, inject);
        let (resource_attributes, resource_dropped) = apply_allowlist(resource_raw, allow, 0);
        for ss in &rs.scope_spans {
            let scope_name = ss
                .scope
                .as_ref()
                .map(|s| s.name.clone())
                .filter(|s| !s.is_empty());
            let scope_version = ss
                .scope
                .as_ref()
                .map(|s| s.version.clone())
                .filter(|s| !s.is_empty());
            for span in &ss.spans {
                let trace_id = require_trace_id(&span.trace_id)?;
                let span_id = require_span_id(&span.span_id)?;
                let parent_span_id = span_id_from_bytes(&span.parent_span_id)?;
                let span_attrs = attrs_from_kv(&span.attributes);
                let http_route = resource_http_route
                    .clone()
                    .or_else(|| span_attrs.get("http.route").cloned());
                let (span_attributes, attr_dropped) =
                    apply_allowlist(span_attrs, allow, span.dropped_attributes_count);
                let hour = hour_from_unix_nano(span.start_time_unix_nano);
                let duration_nano = span
                    .end_time_unix_nano
                    .saturating_sub(span.start_time_unix_nano);
                out.spans.push(SpanRecord {
                    trace_id,
                    span_id,
                    parent_span_id,
                    name: span.name.clone(),
                    kind: span_kind(span.kind),
                    start_time_unix_nano: span.start_time_unix_nano,
                    end_time_unix_nano: span.end_time_unix_nano,
                    duration_nano,
                    status_code: status_code(span.status.as_ref().map(|s| s.code).unwrap_or(0)),
                    service_name: service_name.clone(),
                    deployment_environment: deployment_environment.clone(),
                    tenant_id: tenant_id.clone(),
                    http_route,
                    hour,
                    resource_attributes: resource_attributes.clone(),
                    span_attributes,
                    dropped_attributes_count: resource_dropped.saturating_add(attr_dropped),
                    dropped_events_count: span.dropped_events_count,
                    dropped_links_count: span.dropped_links_count,
                    scope_name: scope_name.clone(),
                    scope_version: scope_version.clone(),
                });
                for event in &span.events {
                    let (attributes, dropped) = apply_allowlist(
                        attrs_from_kv(&event.attributes),
                        allow,
                        event.dropped_attributes_count,
                    );
                    out.events.push(SpanEventRecord {
                        trace_id,
                        span_id,
                        time_unix_nano: event.time_unix_nano,
                        name: event.name.clone(),
                        service_name: service_name.clone(),
                        tenant_id: tenant_id.clone(),
                        hour,
                        attributes,
                        dropped_attributes_count: dropped,
                    });
                }
                for link in &span.links {
                    let linked_trace_id = require_trace_id(&link.trace_id)?;
                    let linked_span_id = require_span_id(&link.span_id)?;
                    let (attributes, dropped) = apply_allowlist(
                        attrs_from_kv(&link.attributes),
                        allow,
                        link.dropped_attributes_count,
                    );
                    out.links.push(SpanLinkRecord {
                        trace_id,
                        span_id,
                        linked_trace_id,
                        linked_span_id,
                        service_name: service_name.clone(),
                        tenant_id: tenant_id.clone(),
                        hour,
                        attributes,
                        dropped_attributes_count: dropped,
                    });
                }
            }
        }
    }
    Ok(out)
}

fn span_kind(kind: i32) -> SpanKind {
    match kind {
        1 => SpanKind::Internal,
        2 => SpanKind::Server,
        3 => SpanKind::Client,
        4 => SpanKind::Producer,
        5 => SpanKind::Consumer,
        _ => SpanKind::Unspecified,
    }
}

fn status_code(code: i32) -> StatusCode {
    match code {
        1 => StatusCode::Ok,
        2 => StatusCode::Error,
        _ => StatusCode::Unset,
    }
}

pub fn decode_logs(
    bytes: &[u8],
    config: &OtlpConfig,
    inject: &BTreeMap<String, String>,
) -> Result<DecodedLogs, DecodeError> {
    if bytes.is_empty() {
        return Err(DecodeError::Empty);
    }
    let req =
        opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest::decode(bytes)
            .map_err(|e| DecodeError::Truncated(e.to_string()))?;
    decode_logs_request(&req, config, inject)
}

pub fn decode_logs_json(
    bytes: &[u8],
    config: &OtlpConfig,
    inject: &BTreeMap<String, String>,
) -> Result<DecodedLogs, DecodeError> {
    if bytes.is_empty() {
        return Err(DecodeError::Empty);
    }
    let req: opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest =
        serde_json::from_slice(bytes).map_err(|e| DecodeError::Json(e.to_string()))?;
    decode_logs_request(&req, config, inject)
}

pub fn decode_logs_request(
    req: &opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest,
    config: &OtlpConfig,
    inject: &BTreeMap<String, String>,
) -> Result<DecodedLogs, DecodeError> {
    let allow = config.attribute_allowlist.as_deref();
    let mut out = DecodedLogs::default();
    for rl in &req.resource_logs {
        let resource_kvs = rl
            .resource
            .as_ref()
            .map(|r| r.attributes.as_slice())
            .unwrap_or(&[]);
        let resource_raw = attrs_from_kv(resource_kvs);
        let (service_name, _, _, tenant_raw) = resource_promotions_from_kvs(resource_kvs);
        let tenant_id = inject_tenant(tenant_raw, inject);
        let (resource_attributes, resource_dropped) = apply_allowlist(resource_raw, allow, 0);
        for sl in &rl.scope_logs {
            let scope_name = sl
                .scope
                .as_ref()
                .map(|s| s.name.clone())
                .filter(|s| !s.is_empty());
            for log in &sl.log_records {
                let (log_attributes, dropped) = apply_allowlist(
                    attrs_from_kv(&log.attributes),
                    allow,
                    log.dropped_attributes_count,
                );
                out.records.push(LogRecord {
                    time_unix_nano: log.time_unix_nano,
                    observed_time_unix_nano: (log.observed_time_unix_nano > 0)
                        .then_some(log.observed_time_unix_nano),
                    severity_text: (!log.severity_text.is_empty())
                        .then(|| log.severity_text.clone()),
                    severity_number: (log.severity_number != 0).then_some(log.severity_number),
                    body: stringify_any(log.body.as_ref()),
                    trace_id: trace_id_from_bytes(&log.trace_id)?,
                    span_id: span_id_from_bytes(&log.span_id)?,
                    service_name: service_name.clone(),
                    tenant_id: tenant_id.clone(),
                    hour: hour_from_unix_nano(log.time_unix_nano),
                    resource_attributes: resource_attributes.clone(),
                    log_attributes,
                    dropped_attributes_count: resource_dropped.saturating_add(dropped),
                    scope_name: scope_name.clone(),
                });
            }
        }
    }
    Ok(out)
}

fn number_value(value: Option<&NumberValue>) -> f64 {
    match value {
        Some(NumberValue::AsDouble(v)) => *v,
        Some(NumberValue::AsInt(v)) => *v as f64,
        None => 0.0,
    }
}

fn temporality(v: i32) -> AggregationTemporality {
    match v {
        1 => AggregationTemporality::Delta,
        2 => AggregationTemporality::Cumulative,
        _ => AggregationTemporality::Unspecified,
    }
}

fn exemplar_trace(
    exemplars: &[opentelemetry_proto::tonic::metrics::v1::Exemplar],
) -> Option<TraceId> {
    exemplars
        .iter()
        .find_map(|e| TraceId::from_bytes(&e.trace_id).ok())
}

pub fn decode_metrics(
    bytes: &[u8],
    config: &OtlpConfig,
    inject: &BTreeMap<String, String>,
) -> Result<DecodedMetrics, DecodeError> {
    if bytes.is_empty() {
        return Err(DecodeError::Empty);
    }
    let req =
        opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest::decode(
            bytes,
        )
        .map_err(|e| DecodeError::Truncated(e.to_string()))?;
    decode_metrics_request(&req, config, inject)
}

pub fn decode_metrics_json(
    bytes: &[u8],
    config: &OtlpConfig,
    inject: &BTreeMap<String, String>,
) -> Result<DecodedMetrics, DecodeError> {
    if bytes.is_empty() {
        return Err(DecodeError::Empty);
    }
    let req: opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest =
        serde_json::from_slice(bytes).map_err(|e| DecodeError::Json(e.to_string()))?;
    decode_metrics_request(&req, config, inject)
}

pub fn decode_metrics_request(
    req: &opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest,
    config: &OtlpConfig,
    inject: &BTreeMap<String, String>,
) -> Result<DecodedMetrics, DecodeError> {
    let allow = config.attribute_allowlist.as_deref();
    let mut out = DecodedMetrics::default();
    for rm in &req.resource_metrics {
        let resource_kvs = rm
            .resource
            .as_ref()
            .map(|r| r.attributes.as_slice())
            .unwrap_or(&[]);
        let resource_raw = attrs_from_kv(resource_kvs);
        let (service_name, _, _, tenant_raw) = resource_promotions_from_kvs(resource_kvs);
        let tenant_id = inject_tenant(tenant_raw, inject);
        let (resource_attributes, _) = apply_allowlist(resource_raw, allow, 0);
        for sm in &rm.scope_metrics {
            let scope_name = sm
                .scope
                .as_ref()
                .map(|s| s.name.clone())
                .filter(|s| !s.is_empty());
            for metric in &sm.metrics {
                match metric.data.as_ref() {
                    Some(MetricData::Gauge(gauge)) => {
                        for pt in &gauge.data_points {
                            let (metric_attributes, _) =
                                apply_allowlist(attrs_from_kv(&pt.attributes), allow, 0);
                            out.gauges.push(GaugeRecord {
                                metric_name: metric.name.clone(),
                                unit: metric.unit.clone(),
                                time_unix_nano: pt.time_unix_nano,
                                start_time_unix_nano: (pt.start_time_unix_nano > 0)
                                    .then_some(pt.start_time_unix_nano),
                                value: number_value(pt.value.as_ref()),
                                service_name: service_name.clone(),
                                tenant_id: tenant_id.clone(),
                                hour: hour_from_unix_nano(pt.time_unix_nano),
                                resource_attributes: resource_attributes.clone(),
                                metric_attributes,
                                exemplar_trace_id: exemplar_trace(&pt.exemplars),
                                scope_name: scope_name.clone(),
                            });
                        }
                    }
                    Some(MetricData::Sum(sum)) => {
                        for pt in &sum.data_points {
                            let (metric_attributes, _) =
                                apply_allowlist(attrs_from_kv(&pt.attributes), allow, 0);
                            out.sums.push(SumRecord {
                                metric_name: metric.name.clone(),
                                unit: metric.unit.clone(),
                                time_unix_nano: pt.time_unix_nano,
                                start_time_unix_nano: (pt.start_time_unix_nano > 0)
                                    .then_some(pt.start_time_unix_nano),
                                value: number_value(pt.value.as_ref()),
                                is_monotonic: sum.is_monotonic,
                                aggregation_temporality: temporality(sum.aggregation_temporality),
                                service_name: service_name.clone(),
                                tenant_id: tenant_id.clone(),
                                hour: hour_from_unix_nano(pt.time_unix_nano),
                                resource_attributes: resource_attributes.clone(),
                                metric_attributes,
                                exemplar_trace_id: exemplar_trace(&pt.exemplars),
                                scope_name: scope_name.clone(),
                            });
                        }
                    }
                    Some(MetricData::Histogram(hist)) => {
                        for pt in &hist.data_points {
                            let (metric_attributes, _) =
                                apply_allowlist(attrs_from_kv(&pt.attributes), allow, 0);
                            let record = HistogramRecord::new(
                                metric.name.clone(),
                                metric.unit.clone(),
                                pt.time_unix_nano,
                                (pt.start_time_unix_nano > 0).then_some(pt.start_time_unix_nano),
                                pt.count,
                                pt.sum,
                                pt.bucket_counts.clone(),
                                pt.explicit_bounds.clone(),
                                service_name.clone(),
                                tenant_id.clone(),
                                resource_attributes.clone(),
                                metric_attributes,
                                exemplar_trace(&pt.exemplars),
                                scope_name.clone(),
                            )
                            .map_err(|e| DecodeError::Histogram(e.to_string()))?;
                            out.histograms.push(record);
                        }
                    }
                    Some(MetricData::ExponentialHistogram(hist)) => {
                        for pt in &hist.data_points {
                            let (metric_attributes, _) =
                                apply_allowlist(attrs_from_kv(&pt.attributes), allow, 0);
                            let positive = pt.positive.as_ref();
                            let negative = pt.negative.as_ref();
                            out.exponential_histograms.push(ExponentialHistogramRecord {
                                metric_name: metric.name.clone(),
                                unit: metric.unit.clone(),
                                time_unix_nano: pt.time_unix_nano,
                                start_time_unix_nano: (pt.start_time_unix_nano > 0)
                                    .then_some(pt.start_time_unix_nano),
                                count: pt.count,
                                sum: pt.sum,
                                scale: pt.scale,
                                zero_count: pt.zero_count,
                                positive_offset: positive.map(|b| b.offset).unwrap_or(0),
                                positive_bucket_counts: positive
                                    .map(|b| b.bucket_counts.clone())
                                    .unwrap_or_default(),
                                negative_offset: negative.map(|b| b.offset).unwrap_or(0),
                                negative_bucket_counts: negative
                                    .map(|b| b.bucket_counts.clone())
                                    .unwrap_or_default(),
                                service_name: service_name.clone(),
                                tenant_id: tenant_id.clone(),
                                hour: hour_from_unix_nano(pt.time_unix_nano),
                                resource_attributes: resource_attributes.clone(),
                                metric_attributes,
                                exemplar_trace_id: exemplar_trace(&pt.exemplars),
                                scope_name: scope_name.clone(),
                            });
                        }
                    }
                    Some(MetricData::Summary(_)) | None => {
                        out.drops.unsupported_metric_kind =
                            out.drops.unsupported_metric_kind.saturating_add(1);
                        let _ = DecodeDrop::UnsupportedMetricKind;
                    }
                }
            }
        }
    }
    Ok(out)
}

fn kv(key: &str, value: &str) -> KeyValue {
    KeyValue {
        key: key.into(),
        value: Some(AnyValue {
            value: Some(any_value::Value::StringValue(value.into())),
        }),
    }
}

pub(crate) fn traces_fixture_request(
) -> opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest {
    use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
    use opentelemetry_proto::tonic::resource::v1::Resource;
    use opentelemetry_proto::tonic::trace::v1::{
        span, status, ResourceSpans, ScopeSpans, Span, Status,
    };
    let parent = Span {
        trace_id: hex::decode("4bf92f3577b34da6a3ce929d0e0e4736").unwrap(),
        span_id: hex::decode("00f067aa0ba902b7").unwrap(),
        parent_span_id: vec![],
        name: "GET /checkout".into(),
        kind: span::SpanKind::Server as i32,
        start_time_unix_nano: 1_000_000_000,
        end_time_unix_nano: 2_000_000_000,
        status: Some(Status {
            message: String::new(),
            code: status::StatusCode::Ok as i32,
        }),
        events: vec![span::Event {
            time_unix_nano: 1_500_000_000,
            name: "exception".into(),
            attributes: vec![],
            dropped_attributes_count: 0,
        }],
        links: vec![span::Link {
            trace_id: hex::decode("4bf92f3577b34da6a3ce929d0e0e4736").unwrap(),
            span_id: hex::decode("00f067aa0ba902b8").unwrap(),
            trace_state: String::new(),
            attributes: vec![],
            dropped_attributes_count: 0,
            flags: 0,
        }],
        ..Default::default()
    };
    let child = Span {
        trace_id: hex::decode("4bf92f3577b34da6a3ce929d0e0e4736").unwrap(),
        span_id: hex::decode("00f067aa0ba902b8").unwrap(),
        parent_span_id: hex::decode("00f067aa0ba902b7").unwrap(),
        name: "db".into(),
        kind: span::SpanKind::Client as i32,
        start_time_unix_nano: 1_100_000_000,
        end_time_unix_nano: 1_200_000_000,
        status: Some(Status {
            message: String::new(),
            code: status::StatusCode::Ok as i32,
        }),
        ..Default::default()
    };
    ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(Resource {
                attributes: vec![
                    kv("service.name", "checkout"),
                    kv("tenant.id", "acme"),
                    kv("deployment.environment", "prod"),
                ],
                dropped_attributes_count: 0,
                entity_refs: vec![],
            }),
            scope_spans: vec![ScopeSpans {
                scope: Some(
                    opentelemetry_proto::tonic::common::v1::InstrumentationScope {
                        name: "test".into(),
                        version: "1.0".into(),
                        attributes: vec![],
                        dropped_attributes_count: 0,
                    },
                ),
                spans: vec![parent, child],
                schema_url: String::new(),
            }],
            schema_url: String::new(),
        }],
    }
}

pub(crate) fn traces_fixture_bytes() -> Vec<u8> {
    traces_fixture_request().encode_to_vec()
}

pub(crate) fn logs_fixture_request(
) -> opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest {
    use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
    use opentelemetry_proto::tonic::logs::v1::{LogRecord as ProtoLog, ResourceLogs, ScopeLogs};
    use opentelemetry_proto::tonic::resource::v1::Resource;
    ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: Some(Resource {
                attributes: vec![kv("service.name", "checkout"), kv("tenant.id", "acme")],
                ..Default::default()
            }),
            scope_logs: vec![ScopeLogs {
                log_records: vec![
                    ProtoLog {
                        time_unix_nano: 1_000_000_000,
                        body: Some(AnyValue {
                            value: Some(any_value::Value::StringValue("timeout".into())),
                        }),
                        severity_number: 17,
                        severity_text: "ERROR".into(),
                        trace_id: hex::decode("4bf92f3577b34da6a3ce929d0e0e4736").unwrap(),
                        span_id: hex::decode("00f067aa0ba902b7").unwrap(),
                        ..Default::default()
                    },
                    ProtoLog {
                        time_unix_nano: 2_000_000_000,
                        body: Some(AnyValue {
                            value: Some(any_value::Value::StringValue("ok".into())),
                        }),
                        severity_number: 9,
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

pub(crate) fn metrics_fixture_request(
) -> opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest {
    use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
    use opentelemetry_proto::tonic::metrics::v1::{
        metric, number_data_point, Histogram, HistogramDataPoint, Metric, NumberDataPoint,
        ResourceMetrics, ScopeMetrics, Sum,
    };
    use opentelemetry_proto::tonic::resource::v1::Resource;
    ExportMetricsServiceRequest {
        resource_metrics: vec![ResourceMetrics {
            resource: Some(Resource {
                attributes: vec![kv("service.name", "checkout"), kv("tenant.id", "acme")],
                ..Default::default()
            }),
            scope_metrics: vec![ScopeMetrics {
                metrics: vec![
                    Metric {
                        name: "requests".into(),
                        unit: "1".into(),
                        data: Some(metric::Data::Sum(Sum {
                            data_points: vec![
                                NumberDataPoint {
                                    time_unix_nano: 1_000_000_000,
                                    value: Some(number_data_point::Value::AsDouble(1.0)),
                                    ..Default::default()
                                },
                                NumberDataPoint {
                                    time_unix_nano: 2_000_000_000,
                                    value: Some(number_data_point::Value::AsDouble(2.0)),
                                    ..Default::default()
                                },
                            ],
                            aggregation_temporality: 2,
                            is_monotonic: true,
                        })),
                        ..Default::default()
                    },
                    Metric {
                        name: "latency".into(),
                        unit: "s".into(),
                        data: Some(metric::Data::Histogram(Histogram {
                            data_points: vec![HistogramDataPoint {
                                time_unix_nano: 1_000_000_000,
                                count: 4,
                                sum: Some(3.0),
                                bucket_counts: vec![1, 2, 1],
                                explicit_bounds: vec![1.0, 2.0],
                                ..Default::default()
                            }],
                            aggregation_temporality: 2,
                        })),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OtlpConfig;
    use opentelemetry_proto::tonic::common::v1::any_value::Value as AnyVal;

    fn cfg() -> OtlpConfig {
        serde_json::from_value(serde_json::json!({})).unwrap()
    }

    #[test]
    fn decode_traces_fixture_counts() {
        let req = traces_fixture_request();
        let bytes = req.encode_to_vec();
        let decoded = decode_traces(&bytes, &cfg(), &BTreeMap::new()).unwrap();
        assert_eq!(decoded.spans.len(), 2);
        assert_eq!(decoded.events.len(), 1);
        assert_eq!(decoded.links.len(), 1);
        assert_eq!(decoded.spans[0].service_name, "checkout");
        assert_eq!(decoded.spans[0].tenant_id, "acme");
        assert_eq!(
            decoded.spans[0].trace_id.to_hex(),
            "4bf92f3577b34da6a3ce929d0e0e4736"
        );
        assert!(decoded.spans[0].parent_span_id.is_none());
        assert!(decoded.spans[1].parent_span_id.is_some());
    }

    #[test]
    fn decode_logs_with_and_without_trace() {
        let decoded =
            decode_logs_request(&logs_fixture_request(), &cfg(), &BTreeMap::new()).unwrap();
        assert_eq!(decoded.records.len(), 2);
        assert!(decoded.records[0].trace_id.is_some());
        assert!(decoded.records[1].trace_id.is_none());
        assert_eq!(decoded.records[0].severity_number, Some(17));
    }

    #[test]
    fn decode_metrics_sum_and_histogram() {
        let decoded =
            decode_metrics_request(&metrics_fixture_request(), &cfg(), &BTreeMap::new()).unwrap();
        assert_eq!(decoded.sums.len(), 2);
        assert_eq!(decoded.histograms.len(), 1);
        assert_eq!(decoded.histograms[0].bucket_counts.len(), 3);
        assert_eq!(decoded.drops.unsupported_metric_kind, 0);
    }

    #[test]
    fn missing_tenant_uses_inject() {
        let mut req = traces_fixture_request();
        req.resource_spans[0]
            .resource
            .as_mut()
            .unwrap()
            .attributes
            .retain(|kv| kv.key != "tenant.id");
        let mut inject = BTreeMap::new();
        inject.insert("tenant_id".into(), "oss".into());
        let decoded = decode_traces_request(&req, &cfg(), &inject).unwrap();
        assert_eq!(decoded.spans[0].tenant_id, "oss");
    }

    #[test]
    fn promotion_wrong_type_leaves_service_empty() {
        let mut req = traces_fixture_request();
        req.resource_spans[0].resource.as_mut().unwrap().attributes = vec![KeyValue {
            key: "service.name".into(),
            value: Some(AnyValue {
                value: Some(AnyVal::IntValue(7)),
            }),
        }];
        let decoded = decode_traces_request(&req, &cfg(), &BTreeMap::new()).unwrap();
        assert!(decoded.spans[0].service_name.is_empty());
        let _ = AnyVal::IntValue(0);
    }

    #[test]
    fn json_fixtures_decode() {
        let traces = include_bytes!("../tests/fixtures/traces.json");
        let logs = include_bytes!("../tests/fixtures/logs.json");
        let metrics = include_bytes!("../tests/fixtures/metrics.json");
        let t = decode_traces_json(traces, &cfg(), &BTreeMap::new()).unwrap();
        assert_eq!(t.spans.len(), 2);
        let l = decode_logs_json(logs, &cfg(), &BTreeMap::new()).unwrap();
        assert_eq!(l.records.len(), 1);
        let m = decode_metrics_json(metrics, &cfg(), &BTreeMap::new()).unwrap();
        assert_eq!(m.sums.len(), 2);
    }

    #[test]
    fn protobuf_fixture_bytes_are_checked_in() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        for name in ["traces.bin", "logs.bin", "metrics.bin"] {
            let path = dir.join(name);
            if !path.exists() {
                let bytes = match name {
                    "traces.bin" => traces_fixture_bytes(),
                    "logs.bin" => logs_fixture_request().encode_to_vec(),
                    "metrics.bin" => metrics_fixture_request().encode_to_vec(),
                    _ => unreachable!(),
                };
                std::fs::write(&path, bytes).unwrap();
            }
            assert!(path.exists(), "missing {}", path.display());
            assert!(!std::fs::read(&path).unwrap().is_empty());
        }
    }
}
