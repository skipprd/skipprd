use std::collections::BTreeMap;
use std::fmt;

use serde_derive::{Deserialize, Serialize};

pub const NANOS_PER_HOUR: u64 = 3_600_000_000_000;

pub const PROMOTED_RESOURCE_KEYS: &[(&str, &str)] = &[
    ("service.name", "service_name"),
    ("deployment.environment", "deployment_environment"),
    ("http.route", "http_route"),
    ("tenant.id", "tenant_id"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdError {
    OddLength,
    InvalidHex,
    WrongLength { expected: usize, actual: usize },
}

impl fmt::Display for IdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OddLength => write!(f, "hex id has odd length"),
            Self::InvalidHex => write!(f, "hex id is not valid hexadecimal"),
            Self::WrongLength { expected, actual } => {
                write!(f, "id length {actual} != {expected}")
            }
        }
    }
}

impl std::error::Error for IdError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TraceId(#[serde(with = "hex_bytes_16")] [u8; 16]);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SpanId(#[serde(with = "hex_bytes_8")] [u8; 8]);

impl TraceId {
    pub fn new(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, IdError> {
        let arr: [u8; 16] = bytes.try_into().map_err(|_| IdError::WrongLength {
            expected: 16,
            actual: bytes.len(),
        })?;
        Ok(Self(arr))
    }

    pub fn from_hex(s: &str) -> Result<Self, IdError> {
        Self::from_bytes(&parse_hex(s, 16)?)
    }

    pub fn to_hex(self) -> String {
        hex::encode(self.0)
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl SpanId {
    pub fn new(bytes: [u8; 8]) -> Self {
        Self(bytes)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, IdError> {
        let arr: [u8; 8] = bytes.try_into().map_err(|_| IdError::WrongLength {
            expected: 8,
            actual: bytes.len(),
        })?;
        Ok(Self(arr))
    }

    pub fn from_hex(s: &str) -> Result<Self, IdError> {
        Self::from_bytes(&parse_hex(s, 8)?)
    }

    pub fn to_hex(self) -> String {
        hex::encode(self.0)
    }

    pub fn as_bytes(&self) -> &[u8; 8] {
        &self.0
    }
}

fn parse_hex(s: &str, expected_bytes: usize) -> Result<Vec<u8>, IdError> {
    if s.len() % 2 != 0 {
        return Err(IdError::OddLength);
    }
    let bytes = hex::decode(s).map_err(|_| IdError::InvalidHex)?;
    if bytes.len() != expected_bytes {
        return Err(IdError::WrongLength {
            expected: expected_bytes,
            actual: bytes.len(),
        });
    }
    Ok(bytes)
}

mod hex_bytes_16 {
    use super::TraceId;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(bytes: &[u8; 16], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&TraceId(*bytes).to_hex())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<[u8; 16], D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        TraceId::from_hex(&s)
            .map(|id| id.0)
            .map_err(serde::de::Error::custom)
    }
}

mod hex_bytes_8 {
    use super::SpanId;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(bytes: &[u8; 8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&SpanId(*bytes).to_hex())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<[u8; 8], D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        SpanId::from_hex(&s)
            .map(|id| id.0)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SpanKind {
    Unspecified,
    Internal,
    Server,
    Client,
    Producer,
    Consumer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StatusCode {
    Unset,
    Ok,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AggregationTemporality {
    Unspecified,
    Delta,
    Cumulative,
}

pub fn hour_from_unix_nano(ts: u64) -> u32 {
    (ts / NANOS_PER_HOUR) as u32
}

pub fn filter_attrs(
    map: BTreeMap<String, String>,
    allowlist: Option<&[String]>,
) -> (BTreeMap<String, String>, u32) {
    match allowlist {
        None => (map, 0),
        Some(list) => {
            let keep: std::collections::HashSet<&str> = list.iter().map(String::as_str).collect();
            let original_len = map.len();
            let filtered: BTreeMap<String, String> = map
                .into_iter()
                .filter(|(k, _)| keep.contains(k.as_str()))
                .collect();
            let dropped = (original_len.saturating_sub(filtered.len())) as u32;
            (filtered, dropped)
        }
    }
}

pub fn promoted_string(resource: &BTreeMap<String, String>, otel_key: &str) -> Option<String> {
    resource.get(otel_key).cloned().filter(|s| !s.is_empty())
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpanRecord {
    pub trace_id: TraceId,
    pub span_id: SpanId,
    pub parent_span_id: Option<SpanId>,
    pub name: String,
    pub kind: SpanKind,
    pub start_time_unix_nano: u64,
    pub end_time_unix_nano: u64,
    pub duration_nano: u64,
    pub status_code: StatusCode,
    pub service_name: String,
    pub deployment_environment: Option<String>,
    pub tenant_id: String,
    pub http_route: Option<String>,
    pub hour: u32,
    pub resource_attributes: BTreeMap<String, String>,
    pub span_attributes: BTreeMap<String, String>,
    pub dropped_attributes_count: u32,
    pub dropped_events_count: u32,
    pub dropped_links_count: u32,
    pub scope_name: Option<String>,
    pub scope_version: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpanEventRecord {
    pub trace_id: TraceId,
    pub span_id: SpanId,
    pub time_unix_nano: u64,
    pub name: String,
    pub service_name: String,
    pub tenant_id: String,
    pub hour: u32,
    pub attributes: BTreeMap<String, String>,
    pub dropped_attributes_count: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpanLinkRecord {
    pub trace_id: TraceId,
    pub span_id: SpanId,
    pub linked_trace_id: TraceId,
    pub linked_span_id: SpanId,
    pub service_name: String,
    pub tenant_id: String,
    pub hour: u32,
    pub attributes: BTreeMap<String, String>,
    pub dropped_attributes_count: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogRecord {
    pub time_unix_nano: u64,
    pub observed_time_unix_nano: Option<u64>,
    pub severity_text: Option<String>,
    pub severity_number: Option<i32>,
    pub body: String,
    pub trace_id: Option<TraceId>,
    pub span_id: Option<SpanId>,
    pub service_name: String,
    pub tenant_id: String,
    pub hour: u32,
    pub resource_attributes: BTreeMap<String, String>,
    pub log_attributes: BTreeMap<String, String>,
    pub dropped_attributes_count: u32,
    pub scope_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GaugeRecord {
    pub metric_name: String,
    pub unit: String,
    pub time_unix_nano: u64,
    pub start_time_unix_nano: Option<u64>,
    pub value: f64,
    pub service_name: String,
    pub tenant_id: String,
    pub hour: u32,
    pub resource_attributes: BTreeMap<String, String>,
    pub metric_attributes: BTreeMap<String, String>,
    pub exemplar_trace_id: Option<TraceId>,
    pub scope_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SumRecord {
    pub metric_name: String,
    pub unit: String,
    pub time_unix_nano: u64,
    pub start_time_unix_nano: Option<u64>,
    pub value: f64,
    pub is_monotonic: bool,
    pub aggregation_temporality: AggregationTemporality,
    pub service_name: String,
    pub tenant_id: String,
    pub hour: u32,
    pub resource_attributes: BTreeMap<String, String>,
    pub metric_attributes: BTreeMap<String, String>,
    pub exemplar_trace_id: Option<TraceId>,
    pub scope_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistogramError {
    BoundsMismatch { counts: usize, bounds: usize },
}

impl fmt::Display for HistogramError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BoundsMismatch { counts, bounds } => write!(
                f,
                "histogram bucket_counts len {counts} must be explicit_bounds len {bounds} + 1"
            ),
        }
    }
}

impl std::error::Error for HistogramError {}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistogramRecord {
    pub metric_name: String,
    pub unit: String,
    pub time_unix_nano: u64,
    pub start_time_unix_nano: Option<u64>,
    pub count: u64,
    pub sum: Option<f64>,
    pub bucket_counts: Vec<u64>,
    pub explicit_bounds: Vec<f64>,
    pub service_name: String,
    pub tenant_id: String,
    pub hour: u32,
    pub resource_attributes: BTreeMap<String, String>,
    pub metric_attributes: BTreeMap<String, String>,
    pub exemplar_trace_id: Option<TraceId>,
    pub scope_name: Option<String>,
}

impl HistogramRecord {
    pub fn new(
        metric_name: String,
        unit: String,
        time_unix_nano: u64,
        start_time_unix_nano: Option<u64>,
        count: u64,
        sum: Option<f64>,
        bucket_counts: Vec<u64>,
        explicit_bounds: Vec<f64>,
        service_name: String,
        tenant_id: String,
        resource_attributes: BTreeMap<String, String>,
        metric_attributes: BTreeMap<String, String>,
        exemplar_trace_id: Option<TraceId>,
        scope_name: Option<String>,
    ) -> Result<Self, HistogramError> {
        if bucket_counts.len() != explicit_bounds.len() + 1 {
            return Err(HistogramError::BoundsMismatch {
                counts: bucket_counts.len(),
                bounds: explicit_bounds.len(),
            });
        }
        Ok(Self {
            metric_name,
            unit,
            time_unix_nano,
            start_time_unix_nano,
            count,
            sum,
            bucket_counts,
            explicit_bounds,
            service_name,
            tenant_id,
            hour: hour_from_unix_nano(time_unix_nano),
            resource_attributes,
            metric_attributes,
            exemplar_trace_id,
            scope_name,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExponentialHistogramRecord {
    pub metric_name: String,
    pub unit: String,
    pub time_unix_nano: u64,
    pub start_time_unix_nano: Option<u64>,
    pub count: u64,
    pub sum: Option<f64>,
    pub scale: i32,
    pub zero_count: u64,
    pub positive_offset: i32,
    pub positive_bucket_counts: Vec<u64>,
    pub negative_offset: i32,
    pub negative_bucket_counts: Vec<u64>,
    pub service_name: String,
    pub tenant_id: String,
    pub hour: u32,
    pub resource_attributes: BTreeMap<String, String>,
    pub metric_attributes: BTreeMap<String, String>,
    pub exemplar_trace_id: Option<TraceId>,
    pub scope_name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let id = TraceId::from_hex("4bf92f3577b34da6a3ce929d0e0e4736").unwrap();
        assert_eq!(id.to_hex(), "4bf92f3577b34da6a3ce929d0e0e4736");
        let span = SpanId::from_hex("00f067aa0ba902b7").unwrap();
        assert_eq!(span.to_hex(), "00f067aa0ba902b7");
    }

    #[test]
    fn odd_length_hex_fails() {
        assert!(matches!(TraceId::from_hex("abc"), Err(IdError::OddLength)));
    }

    #[test]
    fn span_serde_field_names() {
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
        let v = serde_json::to_value(&rec).unwrap();
        for key in [
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
            "http_route",
            "hour",
            "resource_attributes",
            "span_attributes",
        ] {
            assert!(v.get(key).is_some(), "missing {key}");
        }
    }

    #[test]
    fn log_trace_id_none_allowed() {
        let rec = LogRecord {
            time_unix_nano: 1,
            observed_time_unix_nano: None,
            severity_text: None,
            severity_number: None,
            body: "hi".into(),
            trace_id: None,
            span_id: None,
            service_name: "svc".into(),
            tenant_id: String::new(),
            hour: 0,
            resource_attributes: BTreeMap::new(),
            log_attributes: BTreeMap::new(),
            dropped_attributes_count: 0,
            scope_name: None,
        };
        let v = serde_json::to_value(&rec).unwrap();
        assert!(v["trace_id"].is_null());
    }

    #[test]
    fn histogram_bounds_mismatch_fails() {
        let err = HistogramRecord::new(
            "h".into(),
            String::new(),
            1,
            None,
            1,
            None,
            vec![1, 1],
            vec![1.0, 2.0],
            "svc".into(),
            String::new(),
            BTreeMap::new(),
            BTreeMap::new(),
            None,
            None,
        )
        .unwrap_err();
        assert!(matches!(err, HistogramError::BoundsMismatch { .. }));
    }

    #[test]
    fn filter_attrs_none_keeps_all() {
        let mut map = BTreeMap::new();
        map.insert("http.route".into(), "/".into());
        map.insert("user.email".into(), "a@b.c".into());
        let (out, dropped) = filter_attrs(map.clone(), None);
        assert_eq!(out, map);
        assert_eq!(dropped, 0);
    }

    #[test]
    fn filter_attrs_drops_unlisted() {
        let mut map = BTreeMap::new();
        map.insert("http.route".into(), "/".into());
        map.insert("user.email".into(), "a@b.c".into());
        let (out, dropped) = filter_attrs(map, Some(&["http.route".into()]));
        assert_eq!(out.len(), 1);
        assert_eq!(dropped, 1);
        assert!(out.contains_key("http.route"));
    }
}
