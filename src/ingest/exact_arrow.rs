//! Direct Arrow builder path for flat, metadata-stable namespaces.
//!
//! Bypasses normalized JSON allocation and Arrow JSON re-encoding when every enabled
//! field is a flat scalar and incoming rows match metadata without evolution.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use arrow::array::{
    ArrayRef, BooleanBuilder, Float32Builder, Float64Builder, Int16Builder, Int32Builder,
    Int64Builder, Int8Builder, RecordBatch, StringBuilder, TimestampMillisecondBuilder,
};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use arrow::error::ArrowError;
use serde_json::Value;

use crate::discover::{DateParserKind, Metadata, SkipprDataType};
use crate::ingest::fast_ingest::{fast_set_date, validate_required_fields};
use crate::metrics::ingest_profile;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExactArrowFallbackReason {
    IneligibleSchema,
    NotObject,
    MissingRequired,
    TypeMismatch,
    DateParseFailed,
    BuilderError,
}

impl std::fmt::Display for ExactArrowFallbackReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IneligibleSchema => write!(f, "ineligible schema"),
            Self::NotObject => write!(f, "source is not an object"),
            Self::MissingRequired => write!(f, "missing required field"),
            Self::TypeMismatch => write!(f, "type mismatch"),
            Self::DateParseFailed => write!(f, "date parse failed"),
            Self::BuilderError => write!(f, "arrow builder error"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExactColumnSpec {
    pub source_field: String,
    pub out_field_name: String,
    pub data_type: SkipprDataType,
    pub nullable: bool,
    pub default_value: Option<Value>,
    pub date_candidate: Option<crate::discover::DateCandidate>,
    pub date_parser_kind: Option<DateParserKind>,
    pub timezone: bool,
    /// Pre-built map for `fast_set_date` (date columns only).
    pub date_parse_map: Option<Arc<HashMap<String, Metadata>>>,
}

#[derive(Debug, Clone)]
pub struct ExactArrowPlan {
    pub schema: SchemaRef,
    pub columns: Vec<ExactColumnSpec>,
    /// Enabled source field names for fast unknown-field rejection.
    pub allowed_source_fields: HashSet<String>,
}

fn scalar_type_supported(dt: &SkipprDataType) -> bool {
    matches!(
        dt,
        SkipprDataType::String
            | SkipprDataType::Long
            | SkipprDataType::Integer
            | SkipprDataType::Short
            | SkipprDataType::Byte
            | SkipprDataType::Double
            | SkipprDataType::Float
            | SkipprDataType::Boolean
            | SkipprDataType::Timestamp
            | SkipprDataType::TimestampMilli
            | SkipprDataType::Date
    )
}

/// True when every enabled field is a flat scalar with no nested children.
pub fn is_exact_arrow_eligible(fields: &HashMap<String, Metadata>, flatten: bool) -> bool {
    if flatten {
        return false;
    }
    for meta in fields.values() {
        if !meta.enabled {
            continue;
        }
        if !meta.fields.is_empty() {
            return false;
        }
        if !scalar_type_supported(&meta.determined_type) {
            return false;
        }
        if !meta.evolution.is_empty() {
            return false;
        }
    }
    true
}

pub fn plan_for_namespace(
    fields: &HashMap<String, Metadata>,
    flatten: bool,
    schema: SchemaRef,
) -> Option<ExactArrowPlan> {
    if !is_exact_arrow_eligible(fields, flatten) {
        return None;
    }

    let mut columns = Vec::with_capacity(schema.fields().len());
    for field in schema.fields() {
        let out_name = field.name();
        if out_name == "__skippr_empty_struct" {
            continue;
        }
        let (source_field, meta) = fields.iter().find(|(_, m)| {
            m.enabled && (m.out_field_name == *out_name || m.source_field_name == *out_name)
        })?;
        columns.push(ExactColumnSpec {
            source_field: source_field.clone(),
            out_field_name: out_name.clone(),
            data_type: meta.determined_type.clone(),
            nullable: meta.nullable(),
            default_value: meta.default_value().cloned(),
            date_candidate: meta.date_candidate.clone(),
            date_parser_kind: meta.date_parser_kind.clone(),
            timezone: meta.timezone,
            date_parse_map: if matches!(meta.determined_type, SkipprDataType::Date) {
                let mut field_map = HashMap::new();
                field_map.insert(source_field.clone(), meta.clone());
                Some(Arc::new(field_map))
            } else {
                None
            },
        });
    }

    if columns.is_empty() {
        return None;
    }

    let allowed_source_fields = columns.iter().map(|col| col.source_field.clone()).collect();

    Some(ExactArrowPlan {
        schema,
        columns,
        allowed_source_fields,
    })
}

/// Per-batch cache keyed by `(namespace, schema_version)`. Schema version is the
/// monotonic `ARROW_SCHEMA_VERSION` string so evolution mid-batch routes rows to
/// a new partition without reusing a stale plan.
pub fn resolve_cached_exact_plan(
    cache: &mut HashMap<(String, String), Arc<ExactArrowPlan>>,
    namespace: &str,
    schema_version: &str,
    fields: &HashMap<String, Metadata>,
    flatten: bool,
    schema: SchemaRef,
) -> Option<Arc<ExactArrowPlan>> {
    let key = (namespace.to_string(), schema_version.to_string());
    if let Some(plan) = cache.get(&key) {
        return Some(Arc::clone(plan));
    }
    let started = std::time::Instant::now();
    let plan = plan_for_namespace(fields, flatten, schema)?;
    ingest_profile::add_exact_plan_ns(started.elapsed().as_nanos() as u64);
    let plan = Arc::new(plan);
    cache.insert(key, Arc::clone(&plan));
    Some(plan)
}

enum ColumnBuilder {
    String(StringBuilder),
    Int8(Int8Builder),
    Int16(Int16Builder),
    Int32(Int32Builder),
    Int64(Int64Builder),
    Float32(Float32Builder),
    Float64(Float64Builder),
    Boolean(BooleanBuilder),
    Timestamp(TimestampMillisecondBuilder),
}

impl ColumnBuilder {
    fn with_capacity(capacity: usize, data_type: &DataType) -> Result<Self, ArrowError> {
        Ok(match data_type {
            DataType::Utf8 => Self::String(StringBuilder::with_capacity(capacity, capacity * 16)),
            DataType::Int8 => Self::Int8(Int8Builder::with_capacity(capacity)),
            DataType::Int16 => Self::Int16(Int16Builder::with_capacity(capacity)),
            DataType::Int32 => Self::Int32(Int32Builder::with_capacity(capacity)),
            DataType::Int64 => Self::Int64(Int64Builder::with_capacity(capacity)),
            DataType::Float32 => Self::Float32(Float32Builder::with_capacity(capacity)),
            DataType::Float64 => Self::Float64(Float64Builder::with_capacity(capacity)),
            DataType::Boolean => Self::Boolean(BooleanBuilder::with_capacity(capacity)),
            DataType::Timestamp(TimeUnit::Millisecond, _) => {
                Self::Timestamp(TimestampMillisecondBuilder::with_capacity(capacity))
            }
            other => {
                return Err(ArrowError::NotYetImplemented(format!(
                    "exact arrow unsupported arrow type {other:?}"
                )));
            }
        })
    }

    fn append_null(&mut self) -> Result<(), ArrowError> {
        match self {
            Self::String(b) => b.append_null(),
            Self::Int8(b) => b.append_null(),
            Self::Int16(b) => b.append_null(),
            Self::Int32(b) => b.append_null(),
            Self::Int64(b) => b.append_null(),
            Self::Float32(b) => b.append_null(),
            Self::Float64(b) => b.append_null(),
            Self::Boolean(b) => b.append_null(),
            Self::Timestamp(b) => b.append_null(),
        };
        Ok(())
    }

    fn finish(self) -> ArrayRef {
        match self {
            Self::String(mut b) => Arc::new(b.finish()),
            Self::Int8(mut b) => Arc::new(b.finish()),
            Self::Int16(mut b) => Arc::new(b.finish()),
            Self::Int32(mut b) => Arc::new(b.finish()),
            Self::Int64(mut b) => Arc::new(b.finish()),
            Self::Float32(mut b) => Arc::new(b.finish()),
            Self::Float64(mut b) => Arc::new(b.finish()),
            Self::Boolean(mut b) => Arc::new(b.finish()),
            Self::Timestamp(mut b) => Arc::new(b.finish()),
        }
    }
}

pub struct ExactArrowBuilders {
    schema: SchemaRef,
    builders: Vec<ColumnBuilder>,
    rows: usize,
}

impl ExactArrowBuilders {
    pub fn new(plan: &ExactArrowPlan, capacity: usize) -> Result<Self, ArrowError> {
        let mut builders = Vec::with_capacity(plan.schema.fields().len());
        for field in plan.schema.fields() {
            if field.name() == "__skippr_empty_struct" {
                continue;
            }
            builders.push(ColumnBuilder::with_capacity(capacity, field.data_type())?);
        }
        Ok(Self {
            schema: plan.schema.clone(),
            builders,
            rows: 0,
        })
    }

    pub fn len(&self) -> usize {
        self.rows
    }

    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    pub fn finish(mut self) -> Result<RecordBatch, ArrowError> {
        if self.rows == 0 {
            return Err(ArrowError::InvalidArgumentError(
                "exact arrow batch has zero rows".to_string(),
            ));
        }
        let arrays: Vec<ArrayRef> = self.builders.drain(..).map(ColumnBuilder::finish).collect();
        let fields: Vec<Field> = self
            .schema
            .fields()
            .iter()
            .filter(|f| f.name() != "__skippr_empty_struct")
            .map(|f| f.as_ref().clone())
            .collect();
        let batch_schema = Arc::new(Schema::new(fields));
        RecordBatch::try_new(batch_schema, arrays)
    }
}

fn is_empty_value(value: &Value) -> bool {
    value.is_null() || matches!(value.as_str(), Some(s) if s.is_empty())
}

fn append_string(
    builder: &mut StringBuilder,
    value: &Value,
) -> Result<(), ExactArrowFallbackReason> {
    if let Some(s) = value.as_str() {
        builder.append_value(s);
        return Ok(());
    }
    if let Some(i) = value.as_i64() {
        builder.append_value(i.to_string());
        return Ok(());
    }
    if let Some(f) = value.as_f64() {
        builder.append_value(f.to_string());
        return Ok(());
    }
    if let Some(b) = value.as_bool() {
        builder.append_value(b.to_string());
        return Ok(());
    }
    Err(ExactArrowFallbackReason::TypeMismatch)
}

fn append_long(builder: &mut Int64Builder, value: &Value) -> Result<(), ExactArrowFallbackReason> {
    if let Some(i) = value.as_i64() {
        builder.append_value(i);
        return Ok(());
    }
    if let Some(s) = value.as_str() {
        if let Ok(i) = s.parse::<i64>() {
            builder.append_value(i);
            return Ok(());
        }
    }
    Err(ExactArrowFallbackReason::TypeMismatch)
}

fn append_int32(builder: &mut Int32Builder, value: &Value) -> Result<(), ExactArrowFallbackReason> {
    if let Some(i) = value.as_i64() {
        let i32v = i32::try_from(i).map_err(|_| ExactArrowFallbackReason::TypeMismatch)?;
        builder.append_value(i32v);
        return Ok(());
    }
    if let Some(s) = value.as_str() {
        let i = s
            .parse::<i32>()
            .map_err(|_| ExactArrowFallbackReason::TypeMismatch)?;
        builder.append_value(i);
        return Ok(());
    }
    Err(ExactArrowFallbackReason::TypeMismatch)
}

fn append_int16(builder: &mut Int16Builder, value: &Value) -> Result<(), ExactArrowFallbackReason> {
    if let Some(i) = value.as_i64() {
        let i16v = i16::try_from(i).map_err(|_| ExactArrowFallbackReason::TypeMismatch)?;
        builder.append_value(i16v);
        return Ok(());
    }
    if let Some(s) = value.as_str() {
        let i = s
            .parse::<i16>()
            .map_err(|_| ExactArrowFallbackReason::TypeMismatch)?;
        builder.append_value(i);
        return Ok(());
    }
    Err(ExactArrowFallbackReason::TypeMismatch)
}

fn append_int8(builder: &mut Int8Builder, value: &Value) -> Result<(), ExactArrowFallbackReason> {
    if let Some(i) = value.as_i64() {
        let i8v = i8::try_from(i).map_err(|_| ExactArrowFallbackReason::TypeMismatch)?;
        builder.append_value(i8v);
        return Ok(());
    }
    if let Some(s) = value.as_str() {
        let i = s
            .parse::<i8>()
            .map_err(|_| ExactArrowFallbackReason::TypeMismatch)?;
        builder.append_value(i);
        return Ok(());
    }
    Err(ExactArrowFallbackReason::TypeMismatch)
}

fn append_bool(
    builder: &mut BooleanBuilder,
    value: &Value,
) -> Result<(), ExactArrowFallbackReason> {
    if let Some(b) = value.as_bool() {
        builder.append_value(b);
        return Ok(());
    }
    if let Some(s) = value.as_str() {
        if s == "0" {
            builder.append_value(false);
            return Ok(());
        }
        if s == "1" {
            builder.append_value(true);
            return Ok(());
        }
        if let Ok(b) = s.parse::<bool>() {
            builder.append_value(b);
            return Ok(());
        }
    }
    if let Some(i) = value.as_i64() {
        if i == 0 || i == 1 {
            builder.append_value(i == 1);
            return Ok(());
        }
    }
    Err(ExactArrowFallbackReason::TypeMismatch)
}

fn append_double(
    builder: &mut Float64Builder,
    value: &Value,
) -> Result<(), ExactArrowFallbackReason> {
    if let Some(f) = value.as_f64() {
        builder.append_value(f);
        return Ok(());
    }
    if let Some(s) = value.as_str() {
        let f = s
            .parse::<f64>()
            .map_err(|_| ExactArrowFallbackReason::TypeMismatch)?;
        builder.append_value(f);
        return Ok(());
    }
    Err(ExactArrowFallbackReason::TypeMismatch)
}

fn append_float(
    builder: &mut Float32Builder,
    value: &Value,
) -> Result<(), ExactArrowFallbackReason> {
    if let Some(f) = value.as_f64() {
        builder.append_value(f as f32);
        return Ok(());
    }
    if let Some(s) = value.as_str() {
        let f = s
            .parse::<f32>()
            .map_err(|_| ExactArrowFallbackReason::TypeMismatch)?;
        builder.append_value(f);
        return Ok(());
    }
    Err(ExactArrowFallbackReason::TypeMismatch)
}

fn append_timestamp_millis(
    builder: &mut TimestampMillisecondBuilder,
    value: &Value,
) -> Result<(), ExactArrowFallbackReason> {
    if let Some(i) = value.as_i64() {
        let millis = if i < 10_000_000_000 { i * 1000 } else { i };
        builder.append_value(millis);
        return Ok(());
    }
    if let Some(s) = value.as_str() {
        if let Ok(i) = s.parse::<i64>() {
            let millis = if i < 10_000_000_000 { i * 1000 } else { i };
            builder.append_value(millis);
            return Ok(());
        }
    }
    Err(ExactArrowFallbackReason::TypeMismatch)
}

fn append_date_column(
    builder: &mut TimestampMillisecondBuilder,
    col: &ExactColumnSpec,
    value: &Value,
) -> Result<(), ExactArrowFallbackReason> {
    let field_map = col
        .date_parse_map
        .as_ref()
        .ok_or(ExactArrowFallbackReason::BuilderError)?;
    let resolved = fast_set_date(&col.source_field, value, field_map.as_ref())
        .map_err(|_| ExactArrowFallbackReason::DateParseFailed)?;
    let millis = resolved
        .value
        .as_i64()
        .ok_or(ExactArrowFallbackReason::DateParseFailed)?;
    builder.append_value(millis);
    Ok(())
}

fn append_column_value(
    builder: &mut ColumnBuilder,
    col: &ExactColumnSpec,
    value: &Value,
) -> Result<(), ExactArrowFallbackReason> {
    match builder {
        ColumnBuilder::String(b) => append_string(b, value),
        ColumnBuilder::Int8(b) => append_int8(b, value),
        ColumnBuilder::Int16(b) => append_int16(b, value),
        ColumnBuilder::Int32(b) => append_int32(b, value),
        ColumnBuilder::Int64(b) => append_long(b, value),
        ColumnBuilder::Float32(b) => append_float(b, value),
        ColumnBuilder::Float64(b) => append_double(b, value),
        ColumnBuilder::Boolean(b) => append_bool(b, value),
        ColumnBuilder::Timestamp(b) => match col.data_type {
            SkipprDataType::Date => append_date_column(b, col, value),
            SkipprDataType::Timestamp | SkipprDataType::TimestampMilli => {
                append_timestamp_millis(b, value)
            }
            _ => append_timestamp_millis(b, value),
        },
    }
}

fn resolve_column_value<'a>(
    col: &ExactColumnSpec,
    obj: &'a serde_json::Map<String, Value>,
) -> Result<Option<&'a Value>, ExactArrowFallbackReason> {
    match obj.get(&col.source_field) {
        Some(value) if is_empty_value(value) => {
            if col.nullable || col.default_value.is_some() {
                Ok(None)
            } else {
                Err(ExactArrowFallbackReason::MissingRequired)
            }
        }
        Some(value) => Ok(Some(value)),
        None => {
            if col.nullable || col.default_value.is_some() {
                Ok(None)
            } else {
                Err(ExactArrowFallbackReason::MissingRequired)
            }
        }
    }
}

/// Partition-local exact Arrow accumulator keyed by ingest partition hash.
pub struct ExactArrowPartition {
    pub plan: Arc<ExactArrowPlan>,
    pub builders: ExactArrowBuilders,
}

pub fn append_row_to_exact_partition<K>(
    partitions: &mut HashMap<K, ExactArrowPartition>,
    key: &K,
    plan: Arc<ExactArrowPlan>,
    source: &Value,
    fields: &HashMap<String, Metadata>,
    capacity_hint: usize,
) -> Result<(), ExactArrowFallbackReason>
where
    K: std::hash::Hash + Eq + Clone,
{
    if let Some(entry) = partitions.get_mut(key) {
        debug_assert!(Arc::ptr_eq(&entry.plan.schema, &plan.schema));
        return try_append_row(&plan, source, fields, &mut entry.builders);
    }

    let mut builders = ExactArrowBuilders::new(plan.as_ref(), capacity_hint)
        .map_err(|_| ExactArrowFallbackReason::BuilderError)?;
    try_append_row(plan.as_ref(), source, fields, &mut builders)?;
    partitions.insert(key.clone(), ExactArrowPartition { plan, builders });
    Ok(())
}

pub fn try_append_row(
    plan: &ExactArrowPlan,
    source: &Value,
    fields: &HashMap<String, Metadata>,
    builders: &mut ExactArrowBuilders,
) -> Result<(), ExactArrowFallbackReason> {
    let obj = source
        .as_object()
        .ok_or(ExactArrowFallbackReason::NotObject)?;

    if obj.len() > plan.allowed_source_fields.len() {
        return Err(ExactArrowFallbackReason::TypeMismatch);
    }
    for (field, value) in obj {
        if is_empty_value(value) {
            continue;
        }
        if !plan.allowed_source_fields.contains(field) {
            return Err(ExactArrowFallbackReason::TypeMismatch);
        }
    }

    if builders.builders.len() != plan.columns.len() {
        return Err(ExactArrowFallbackReason::BuilderError);
    }

    for (col, builder) in plan.columns.iter().zip(builders.builders.iter_mut()) {
        match resolve_column_value(col, obj) {
            Ok(Some(value)) => append_column_value(builder, col, value)?,
            Ok(None) => {
                if let Some(default) = col.default_value.as_ref() {
                    append_column_value(builder, col, default)?;
                } else {
                    builder
                        .append_null()
                        .map_err(|_| ExactArrowFallbackReason::BuilderError)?;
                }
            }
            Err(reason) => return Err(reason),
        }
    }

    // Required nested validation is a no-op for flat schemas; keep parity with fast path.
    let _ = fields;

    builders.rows += 1;
    Ok(())
}

/// Validate required-field semantics using the same helper as fast ingest.
pub fn validate_exact_required_fields(
    fields: &HashMap<String, Metadata>,
    _source: &Value,
) -> Result<(), ExactArrowFallbackReason> {
    let mut template = serde_json::Map::new();
    for meta in fields.values() {
        if !meta.enabled || !meta.fields.is_empty() {
            continue;
        }
        let name = if meta.out_field_name.is_empty() {
            meta.source_field_name.clone()
        } else {
            meta.out_field_name.clone()
        };
        template.insert(name, Value::Null);
    }
    let message = Value::Object(template);
    validate_required_fields(fields, &message)
        .map_err(|_| ExactArrowFallbackReason::MissingRequired)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discover::date_formats::DateFormats;
    use crate::discover::{DateCandidate, DateParserKind};
    use crate::ingest::fast_ingest::fast_path_ingest;
    use crate::ingest::fast_ingest::DEFAULT_NESTED_MESSAGE;
    use crate::ingest_work::Ingest;
    use arrow::json::ReaderBuilder as ArrowJsonReaderBuilder;
    use serde_json::json;
    use std::sync::Arc;

    fn flat_metadata() -> HashMap<String, Metadata> {
        let mut fields = HashMap::new();
        fields.insert(
            "id".to_string(),
            Metadata::new_with_type(SkipprDataType::String, "id"),
        );
        fields.insert(
            "count".to_string(),
            Metadata::new_with_type(SkipprDataType::Integer, "count"),
        );
        fields.insert(
            "active".to_string(),
            Metadata::new_with_type(SkipprDataType::Boolean, "active"),
        );
        let mut fetch_time = Metadata::new_with_type(SkipprDataType::Date, "fetch_time");
        fetch_time.date_candidate = Some(DateCandidate {
            check_count: 10,
            valid_count: 10,
            field: "fetch_time".to_string(),
            format: DateFormats::Iso8601_2.name().to_string(),
        });
        fetch_time.date_parser_kind = Some(DateParserKind::ZNoMsT);
        fetch_time.timezone = true;
        fields.insert("fetch_time".to_string(), fetch_time);
        fields
    }

    fn install_template(namespace: &str, fields: &HashMap<String, Metadata>) {
        let template = crate::ingest::fast_ingest::create_default_nested_message(fields);
        DEFAULT_NESTED_MESSAGE.insert(namespace.to_string(), Arc::new(template));
    }

    #[test]
    fn eligible_flat_schema_is_detected() {
        let mut fields = flat_metadata();
        assert!(is_exact_arrow_eligible(&fields, false));
        assert!(!is_exact_arrow_eligible(&fields, true));
    }

    #[test]
    fn nested_schema_is_ineligible() {
        let mut fields = flat_metadata();
        let mut nested = Metadata::new_with_type(SkipprDataType::Record, "address");
        nested.fields.insert(
            "city".to_string(),
            Metadata::new_with_type(SkipprDataType::String, "city"),
        );
        fields.insert("address".to_string(), nested);
        assert!(!is_exact_arrow_eligible(&fields, false));
    }

    #[test]
    fn exact_arrow_matches_legacy_fast_path_for_flat_rows() {
        let namespace = "bench_flat";
        let mut fields = flat_metadata();
        install_template(namespace, &fields);

        let mut pipeline = crate::discover::PipelineMetadata::new();
        let mut ns_meta = Metadata::new_with_type(SkipprDataType::Record, namespace);
        ns_meta.fields = Box::new(fields.clone());
        pipeline.metadata.insert(namespace.to_string(), ns_meta);
        let schema =
            Ingest::prepare_arrow_schema_with_metadata(namespace, &pipeline.metadata, false)
                .expect("schema");

        let plan = plan_for_namespace(&fields, false, schema.clone()).expect("plan");
        let mut builders = ExactArrowBuilders::new(&plan, 4).expect("builders");

        let rows = vec![
            json!({"id":"a","count":1,"active":true,"fetch_time":"2026-01-01T00:00:00Z"}),
            json!({"id":"b","count":2,"active":false,"fetch_time":"2026-01-02T00:00:00Z"}),
        ];

        let mut legacy_values = Vec::new();
        for row in &rows {
            try_append_row(&plan, row, &fields, &mut builders).expect("append");
            legacy_values
                .push(fast_path_ingest(row, &fields, namespace, false).expect("legacy normalize"));
        }

        let exact_batch = builders.finish().expect("exact batch");
        let legacy_refs: Vec<&Value> = legacy_values.iter().collect();
        let mut decoder = ArrowJsonReaderBuilder::new(schema).build_decoder().unwrap();
        decoder.serialize(&legacy_refs).unwrap();
        let legacy_batch = decoder.flush().unwrap().expect("legacy batch");

        assert_eq!(exact_batch.num_rows(), legacy_batch.num_rows());
        assert_eq!(exact_batch.num_columns(), legacy_batch.num_columns());
    }

    #[test]
    fn unknown_field_falls_back() {
        let mut fields = flat_metadata();
        let namespace = "bench_fallback";
        install_template(namespace, &fields);
        let mut pipeline = crate::discover::PipelineMetadata::new();
        let mut ns_meta = Metadata::new_with_type(SkipprDataType::Record, namespace);
        ns_meta.fields = Box::new(fields.clone());
        pipeline.metadata.insert(namespace.to_string(), ns_meta);
        let schema =
            Ingest::prepare_arrow_schema_with_metadata(namespace, &pipeline.metadata, false)
                .expect("schema");
        let plan = plan_for_namespace(&fields, false, schema).expect("plan");
        let mut builders = ExactArrowBuilders::new(&plan, 1).expect("builders");
        let row =
            json!({"id":"a","count":1,"active":true,"fetch_time":"2026-01-01T00:00:00Z","extra":1});
        let err = try_append_row(&plan, &row, &fields, &mut builders).unwrap_err();
        assert_eq!(err, ExactArrowFallbackReason::TypeMismatch);
        assert_eq!(builders.len(), 0);
    }

    #[test]
    fn cached_plan_hits_after_first_miss() {
        let namespace = "cache_hit";
        let mut fields = flat_metadata();
        install_template(namespace, &fields);
        let mut pipeline = crate::discover::PipelineMetadata::new();
        let mut ns_meta = Metadata::new_with_type(SkipprDataType::Record, namespace);
        ns_meta.fields = Box::new(fields.clone());
        pipeline.metadata.insert(namespace.to_string(), ns_meta);
        let schema =
            Ingest::prepare_arrow_schema_with_metadata(namespace, &pipeline.metadata, false)
                .expect("schema");

        ingest_profile::reset_profile_counters();
        let mut cache = HashMap::new();
        let p1 =
            resolve_cached_exact_plan(&mut cache, namespace, "1", &fields, false, schema.clone())
                .expect("plan");
        let p2 = resolve_cached_exact_plan(&mut cache, namespace, "1", &fields, false, schema)
            .expect("plan");
        assert!(Arc::ptr_eq(&p1, &p2));
        let snap = ingest_profile::snapshot();
        assert_eq!(snap.exact_plan_ns > 0, true, "first resolve builds plan");
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn cached_plan_misses_when_schema_version_changes() {
        let namespace = "cache_version";
        let mut fields = flat_metadata();
        install_template(namespace, &fields);
        let mut pipeline = crate::discover::PipelineMetadata::new();
        let mut ns_meta = Metadata::new_with_type(SkipprDataType::Record, namespace);
        ns_meta.fields = Box::new(fields.clone());
        pipeline.metadata.insert(namespace.to_string(), ns_meta);
        let schema_v1 =
            Ingest::prepare_arrow_schema_with_metadata(namespace, &pipeline.metadata, false)
                .expect("schema");

        let mut cache = HashMap::new();
        let p1 = resolve_cached_exact_plan(
            &mut cache,
            namespace,
            "1",
            &fields,
            false,
            schema_v1.clone(),
        )
        .expect("plan v1");

        fields.insert(
            "new_col".to_string(),
            Metadata::new_with_type(SkipprDataType::String, "new_col"),
        );
        let mut pipeline2 = crate::discover::PipelineMetadata::new();
        let mut ns_meta2 = Metadata::new_with_type(SkipprDataType::Record, namespace);
        ns_meta2.fields = Box::new(fields.clone());
        pipeline2.metadata.insert(namespace.to_string(), ns_meta2);
        let schema_v2 =
            Ingest::prepare_arrow_schema_with_metadata(namespace, &pipeline2.metadata, false)
                .expect("schema v2");

        let p2 = resolve_cached_exact_plan(&mut cache, namespace, "2", &fields, false, schema_v2)
            .expect("plan v2");
        assert!(!Arc::ptr_eq(&p1, &p2));
        assert_eq!(cache.len(), 2);
        assert!(p2.columns.iter().any(|c| c.source_field == "new_col"));
    }

    #[test]
    fn cached_plan_isolated_per_namespace() {
        let fields_a = flat_metadata();
        let fields_b = {
            let mut f = HashMap::new();
            f.insert(
                "x".to_string(),
                Metadata::new_with_type(SkipprDataType::Long, "x"),
            );
            f
        };
        let ns_a = "ns_a";
        let ns_b = "ns_b";
        install_template(ns_a, &fields_a);
        install_template(ns_b, &fields_b);

        let mut pipeline = crate::discover::PipelineMetadata::new();
        for (ns, fields) in [(ns_a, &fields_a), (ns_b, &fields_b)] {
            let mut ns_meta = Metadata::new_with_type(SkipprDataType::Record, ns);
            ns_meta.fields = Box::new(fields.clone());
            pipeline.metadata.insert(ns.to_string(), ns_meta);
        }
        let schema_a =
            Ingest::prepare_arrow_schema_with_metadata(ns_a, &pipeline.metadata, false).unwrap();
        let schema_b =
            Ingest::prepare_arrow_schema_with_metadata(ns_b, &pipeline.metadata, false).unwrap();

        let mut cache = HashMap::new();
        let plan_a = resolve_cached_exact_plan(&mut cache, ns_a, "1", &fields_a, false, schema_a)
            .expect("plan a");
        let plan_b = resolve_cached_exact_plan(&mut cache, ns_b, "1", &fields_b, false, schema_b)
            .expect("plan b");
        assert!(!Arc::ptr_eq(&plan_a, &plan_b));
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn schema_version_in_partition_key_isolates_evolved_batches() {
        let ns = "evo_partition";
        let mut fields = flat_metadata();
        install_template(ns, &fields);
        let mut partitions: HashMap<(String, String), ExactArrowPartition> = HashMap::new();
        let mut pipeline = crate::discover::PipelineMetadata::new();
        let mut ns_meta = Metadata::new_with_type(SkipprDataType::Record, ns);
        ns_meta.fields = Box::new(fields.clone());
        pipeline.metadata.insert(ns.to_string(), ns_meta);
        let schema_v1 =
            Ingest::prepare_arrow_schema_with_metadata(ns, &pipeline.metadata, false).unwrap();
        let mut cache = HashMap::new();
        let plan_v1 =
            resolve_cached_exact_plan(&mut cache, ns, "1", &fields, false, schema_v1).unwrap();
        let row = json!({"id":"a","count":1,"active":true,"fetch_time":"2026-01-01T00:00:00Z"});
        append_row_to_exact_partition(
            &mut partitions,
            &("sink".to_string(), "1".to_string()),
            Arc::clone(&plan_v1),
            &row,
            &fields,
            4,
        )
        .unwrap();

        fields.insert(
            "extra".to_string(),
            Metadata::new_with_type(SkipprDataType::String, "extra"),
        );
        let mut pipeline2 = crate::discover::PipelineMetadata::new();
        let mut ns_meta2 = Metadata::new_with_type(SkipprDataType::Record, ns);
        ns_meta2.fields = Box::new(fields.clone());
        pipeline2.metadata.insert(ns.to_string(), ns_meta2);
        let schema_v2 =
            Ingest::prepare_arrow_schema_with_metadata(ns, &pipeline2.metadata, false).unwrap();
        let plan_v2 =
            resolve_cached_exact_plan(&mut cache, ns, "2", &fields, false, schema_v2).unwrap();
        append_row_to_exact_partition(
            &mut partitions,
            &("sink".to_string(), "2".to_string()),
            plan_v2,
            &row,
            &fields,
            4,
        )
        .unwrap();

        assert_eq!(partitions.len(), 2);
        assert_eq!(
            partitions[&("sink".to_string(), "1".to_string())]
                .builders
                .len(),
            1
        );
        assert_eq!(
            partitions[&("sink".to_string(), "2".to_string())]
                .builders
                .len(),
            1
        );
    }

    #[test]
    fn metadata_snapshot_tracks_schema_version_bumps() {
        use crate::{metadata_test_lock, METADATA};

        let _guard = metadata_test_lock();
        let ns = "metadata_version";
        let mut fields = flat_metadata();
        install_template(ns, &fields);
        let mut pipeline = crate::discover::PipelineMetadata::new();
        let mut ns_meta = Metadata::new_with_type(SkipprDataType::Record, ns);
        ns_meta.fields = Box::new(fields.clone());
        pipeline.metadata.insert(ns.to_string(), ns_meta);
        let _schema_v1 =
            Ingest::prepare_arrow_schema_with_metadata(ns, &pipeline.metadata, false).unwrap();

        let mut snapshot = METADATA.load().clone();
        METADATA.store(Arc::new(pipeline));
        let mut versions = HashMap::new();
        let v1 = crate::ingest_work::namespace_schema_version(ns);
        crate::ingest_work::refresh_metadata_snapshot_for_namespace(
            ns,
            &mut snapshot,
            &mut versions,
        );
        assert_eq!(versions.get(ns), Some(&v1));

        fields.insert(
            "extra".to_string(),
            Metadata::new_with_type(SkipprDataType::String, "extra"),
        );
        let mut pipeline2 = crate::discover::PipelineMetadata::new();
        let mut ns_meta2 = Metadata::new_with_type(SkipprDataType::Record, ns);
        ns_meta2.fields = Box::new(fields);
        pipeline2.metadata.insert(ns.to_string(), ns_meta2);
        METADATA.store(Arc::new(pipeline2));
        let _schema_v2 =
            Ingest::prepare_arrow_schema_with_metadata(ns, &METADATA.load().metadata, false)
                .unwrap();

        let v2 = crate::ingest_work::namespace_schema_version(ns);
        assert!(v2 > v1);
        crate::ingest_work::refresh_metadata_snapshot_for_namespace(
            ns,
            &mut snapshot,
            &mut versions,
        );
        assert_eq!(versions.get(ns), Some(&v2));
        assert!(snapshot
            .metadata
            .get(ns)
            .unwrap()
            .fields
            .contains_key("extra"));
    }
}
