use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::SystemTime;

use arrow::array::{ArrayRef, Int64Array, StringArray};
use arrow::record_batch::RecordBatch;
use arrow_schema::{DataType as ArrowDataType, Field as ArrowField, Schema as ArrowSchema};

use crate::discover::{Metadata, SkipprDataType};
use crate::helpers::configuration::Config;
use crate::{ARROW_SCHEMA, ARROW_SCHEMA_VERSION, METADATA};

#[derive(Clone, Debug)]
pub(crate) struct DeadletterRecord {
    pub namespace: String,
    pub record: String,
    pub error: String,
    pub failure_code: String,
    pub event_time: Option<i64>,
    pub source_uri: String,
    pub offset_key: String,
    pub offset_pos: u64,
}

pub(crate) fn table_name() -> String {
    format!("_dl_{}", Config::get_pipeline_name())
}

pub(crate) fn arrow_schema() -> Arc<ArrowSchema> {
    Arc::new(ArrowSchema::new(vec![
        ArrowField::new("id", ArrowDataType::Utf8, false),
        ArrowField::new("namespace", ArrowDataType::Utf8, false),
        ArrowField::new("record", ArrowDataType::Utf8, false),
        ArrowField::new("error", ArrowDataType::Utf8, false),
        ArrowField::new("failure_code", ArrowDataType::Utf8, false),
        ArrowField::new("event_time", ArrowDataType::Int64, true),
        ArrowField::new("processed_time", ArrowDataType::Int64, false),
        ArrowField::new("source_uri", ArrowDataType::Utf8, true),
        ArrowField::new("offset_key", ArrowDataType::Utf8, true),
        ArrowField::new("offset_pos", ArrowDataType::Int64, true),
    ]))
}

pub(crate) fn build_batch(records: &[DeadletterRecord]) -> Option<RecordBatch> {
    if records.is_empty() {
        return None;
    }
    let now_millis = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    let ids: Vec<String> = records
        .iter()
        .map(|r| {
            format!(
                "{:x}",
                md5::compute(format!("{}:{}:{}", r.namespace, r.offset_key, r.offset_pos))
            )
        })
        .collect();
    let namespaces: Vec<&str> = records.iter().map(|r| r.namespace.as_str()).collect();
    let recs: Vec<&str> = records.iter().map(|r| r.record.as_str()).collect();
    let errors: Vec<&str> = records.iter().map(|r| r.error.as_str()).collect();
    let codes: Vec<&str> = records.iter().map(|r| r.failure_code.as_str()).collect();
    let event_times: Vec<Option<i64>> = records.iter().map(|r| r.event_time).collect();
    let processed_times: Vec<i64> = vec![now_millis; records.len()];
    let source_uris: Vec<Option<&str>> = records
        .iter()
        .map(|r| {
            if r.source_uri.is_empty() {
                None
            } else {
                Some(r.source_uri.as_str())
            }
        })
        .collect();
    let offset_keys: Vec<Option<&str>> = records
        .iter()
        .map(|r| {
            if r.offset_key.is_empty() {
                None
            } else {
                Some(r.offset_key.as_str())
            }
        })
        .collect();
    let offset_positions: Vec<Option<i64>> =
        records.iter().map(|r| Some(r.offset_pos as i64)).collect();

    let schema = arrow_schema();
    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(StringArray::from(ids)) as ArrayRef,
            Arc::new(StringArray::from(namespaces)) as ArrayRef,
            Arc::new(StringArray::from(recs)) as ArrayRef,
            Arc::new(StringArray::from(errors)) as ArrayRef,
            Arc::new(StringArray::from(codes)) as ArrayRef,
            Arc::new(Int64Array::from(event_times)) as ArrayRef,
            Arc::new(Int64Array::from(processed_times)) as ArrayRef,
            Arc::new(StringArray::from(source_uris)) as ArrayRef,
            Arc::new(StringArray::from(offset_keys)) as ArrayRef,
            Arc::new(Int64Array::from(offset_positions)) as ArrayRef,
        ],
    )
    .ok()
}

/// Lazily registers the deadletter namespace schema in ARROW_SCHEMA and METADATA
/// so the table is created alongside normal pipeline tables with no special branches.
pub(crate) fn ensure_namespace_registered() {
    let dl_ns = table_name();
    if ARROW_SCHEMA.get(&dl_ns).is_some() {
        return;
    }
    let schema = arrow_schema();
    ARROW_SCHEMA
        .entry(dl_ns.clone())
        .or_insert_with(|| arc_swap::ArcSwap::from(schema));
    ARROW_SCHEMA_VERSION
        .entry(dl_ns.clone())
        .or_insert_with(|| AtomicU64::new(0));

    fn dl_field(name: &str, dt: SkipprDataType) -> Metadata {
        let mut m = Metadata::new().unwrap();
        m.determined_type = dt;
        m.out_field_name = name.to_string();
        m
    }

    let mut fields: HashMap<String, Metadata> = HashMap::new();
    fields.insert("id".into(), dl_field("id", SkipprDataType::String));
    fields.insert(
        "namespace".into(),
        dl_field("namespace", SkipprDataType::String),
    );
    fields.insert("record".into(), dl_field("record", SkipprDataType::String));
    fields.insert("error".into(), dl_field("error", SkipprDataType::String));
    fields.insert(
        "failure_code".into(),
        dl_field("failure_code", SkipprDataType::String),
    );
    fields.insert(
        "event_time".into(),
        dl_field("event_time", SkipprDataType::Long),
    );
    fields.insert(
        "processed_time".into(),
        dl_field("processed_time", SkipprDataType::Long),
    );
    fields.insert(
        "source_uri".into(),
        dl_field("source_uri", SkipprDataType::String),
    );
    fields.insert(
        "offset_key".into(),
        dl_field("offset_key", SkipprDataType::String),
    );
    fields.insert(
        "offset_pos".into(),
        dl_field("offset_pos", SkipprDataType::Long),
    );

    let mut ns_meta = Metadata::new().unwrap();
    ns_meta.fields = Box::new(fields);
    ns_meta.determined_type = SkipprDataType::Record;

    let mut pm = METADATA.load().as_ref().clone();
    pm.metadata.insert(dl_ns.clone(), ns_meta);
    METADATA.store(Arc::new(pm));
}
