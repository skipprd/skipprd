use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::SystemTime;

use arrow::array::{ArrayRef, Int64Array, StringArray};
use arrow::record_batch::RecordBatch;
use arrow_schema::{DataType as ArrowDataType, Field as ArrowField, Schema as ArrowSchema};

use crate::discover::{Metadata, SkipprDataType};
use crate::helpers::configuration::Config;
use crate::runtime_plugins::schema_state::bump_pipeline_schema_version;
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

pub fn table_name(config: &Config) -> String {
    table_name_for_pipeline(&config.get_pipeline_name())
}

pub fn table_name_for_pipeline(pipeline: &str) -> String {
    format!("_dl_{pipeline}")
}

pub fn is_deadletter_namespace(namespace: &str) -> bool {
    namespace.starts_with("_dl_")
}

const DEADLETTER_COLUMNS: &[(&str, SkipprDataType, bool)] = &[
    ("id", SkipprDataType::String, false),
    ("namespace", SkipprDataType::String, false),
    ("record", SkipprDataType::String, false),
    ("error", SkipprDataType::String, false),
    ("failure_code", SkipprDataType::String, false),
    ("event_time", SkipprDataType::Long, true),
    ("processed_time", SkipprDataType::Long, false),
    ("source_uri", SkipprDataType::String, true),
    ("offset_key", SkipprDataType::String, true),
    ("offset_pos", SkipprDataType::Long, true),
];

fn deadletter_arrow_type(data_type: &SkipprDataType) -> Result<ArrowDataType, String> {
    match data_type {
        SkipprDataType::String => Ok(ArrowDataType::Utf8),
        SkipprDataType::Long => Ok(ArrowDataType::Int64),
        other => Err(format!(
            "deadletter column type {} has no Arrow mapping",
            other.as_str()
        )),
    }
}

pub(crate) fn arrow_schema() -> Arc<ArrowSchema> {
    let fields = DEADLETTER_COLUMNS
        .iter()
        .map(|(name, data_type, nullable)| {
            Ok(ArrowField::new(
                *name,
                deadletter_arrow_type(data_type)?,
                *nullable,
            ))
        })
        .collect::<Result<Vec<_>, String>>()
        .expect("deadletter columns map to Arrow");
    Arc::new(ArrowSchema::new(fields))
}

/// One deadletter record. Root id stays 0. Children have distinct ids ≥ 1.
pub(crate) fn namespace_metadata(namespace: &str, schema_id: u64) -> Result<Metadata, String> {
    let mut fields = HashMap::new();
    for (name, data_type, nullable) in DEADLETTER_COLUMNS {
        let mut field = Metadata::new().map_err(|err| err.to_string())?;
        field.determined_type = data_type.clone();
        field.out_field_name = (*name).to_string();
        field.source_field_name = (*name).to_string();
        field.nullable = *nullable;
        fields.insert((*name).to_string(), field);
    }
    let mut root = Metadata::new().map_err(|err| err.to_string())?;
    root.determined_type = SkipprDataType::Record;
    root.fields = Box::new(fields);
    Metadata::migrate_tree(namespace, &mut root, schema_id, Vec::new());
    Metadata::repair_field_identity(namespace, &mut root);
    Metadata::validate_field_identity(namespace, &root)?;
    Ok(root)
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
pub fn ensure_namespace_registered(config: &Config) -> Result<(), String> {
    let dl_ns = table_name(config);
    if ARROW_SCHEMA.get(&dl_ns).is_some() {
        return Ok(());
    }
    let schema = arrow_schema();
    ARROW_SCHEMA
        .entry(dl_ns.clone())
        .or_insert_with(|| arc_swap::ArcSwap::from(schema));
    ARROW_SCHEMA_VERSION
        .entry(dl_ns.clone())
        .or_insert_with(|| AtomicU64::new(0));

    let mut pm = METADATA.load().as_ref().clone();
    let schema_id = if pm.schema_id == 0 { 1 } else { pm.schema_id };
    let ns_meta = namespace_metadata(&dl_ns, schema_id)?;
    pm.metadata.insert(dl_ns.clone(), ns_meta);
    METADATA.store(Arc::new(pm));
    bump_pipeline_schema_version();
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::discover::PipelineMetadata;
    use crate::helpers::schema_store::persist_pipeline_schema;
    use skippr_wal_catalog::{CatalogFence, MemorySkipprStore};

    #[test]
    fn deadletter_namespace_persists_unique_field_ids() {
        let fence = CatalogFence::local();
        let store = MemorySkipprStore::open(fence);
        let mut pipeline = PipelineMetadata::new(&Config::new());
        pipeline.name = "device_data".into();
        let namespace = "_dl_device_data";
        let root = namespace_metadata(namespace, 1).unwrap();
        let ids: Vec<i32> = root.fields.values().map(|field| field.field_id).collect();
        assert_eq!(ids.len(), DEADLETTER_COLUMNS.len());
        assert!(ids.iter().all(|id| *id >= 1), "{ids:?}");
        assert_eq!(ids.iter().collect::<HashSet<_>>().len(), ids.len());
        pipeline.metadata.insert(namespace.to_string(), root);
        persist_pipeline_schema(&store, fence, &pipeline).unwrap();
    }

    #[test]
    fn deadletter_arrow_schema_matches_column_catalog() {
        let schema = arrow_schema();
        assert_eq!(schema.fields().len(), DEADLETTER_COLUMNS.len());
        for (index, (name, data_type, nullable)) in DEADLETTER_COLUMNS.iter().enumerate() {
            let field = schema.field(index);
            assert_eq!(field.name(), *name);
            assert_eq!(
                field.data_type(),
                &deadletter_arrow_type(data_type).unwrap()
            );
            assert_eq!(field.is_nullable(), *nullable);
        }
    }
}
