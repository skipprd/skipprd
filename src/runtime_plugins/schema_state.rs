use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;

use arc_swap::ArcSwap;
use once_cell::sync::Lazy;

use crate::discover::OutputMetadata;
use crate::runtime_plugins::protocol::RuntimeSchemaState;
use crate::{METADATA, PIPELINE_SCHEMA_VERSION};

static RUNTIME_SOURCE_SCHEMA_STATE: Lazy<ArcSwap<RuntimeSchemaState>> = Lazy::new(|| {
    ArcSwap::new(Arc::new(RuntimeSchemaState {
        version: 0,
        namespaces: BTreeMap::new(),
    }))
});
static RUNTIME_SOURCE_SCHEMA_STATE_UPDATE_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

pub fn current_pipeline_schema_version() -> u64 {
    PIPELINE_SCHEMA_VERSION.load(Ordering::Acquire)
}

pub fn bump_pipeline_schema_version() -> u64 {
    PIPELINE_SCHEMA_VERSION.fetch_add(1, Ordering::AcqRel) + 1
}

pub fn clear_runtime_source_schema_state() {
    let _guard = RUNTIME_SOURCE_SCHEMA_STATE_UPDATE_LOCK
        .lock()
        .expect("runtime source schema state update lock poisoned");
    RUNTIME_SOURCE_SCHEMA_STATE.store(Arc::new(RuntimeSchemaState {
        version: 0,
        namespaces: BTreeMap::new(),
    }));
}

fn metadata_schema_state_namespaces() -> BTreeMap<String, OutputMetadata> {
    let metadata = METADATA.load();
    metadata
        .metadata
        .iter()
        .map(|(namespace, schema)| {
            let output = if metadata.flattened {
                OutputMetadata::from_flatterened_metadata(schema)
            } else {
                OutputMetadata::from_metadata(schema)
            };
            (namespace.clone(), output)
        })
        .collect()
}

fn output_schema_equivalent(left: &OutputMetadata, right: &OutputMetadata) -> bool {
    left.out_field_name() == right.out_field_name()
        && left.determined_type() == right.determined_type()
        && left.determined_type_values() == right.determined_type_values()
        && left.nullable() == right.nullable()
        && left.child_fields().count() == right.child_fields().count()
        && left.child_fields().all(|(field_name, left_child)| {
            right
                .child_fields()
                .find(|(right_field_name, _)| *right_field_name == field_name)
                .is_some_and(|(_, right_child)| output_schema_equivalent(left_child, right_child))
        })
}

pub fn apply_runtime_source_schema_state(schema_state: RuntimeSchemaState) -> Vec<String> {
    let _guard = RUNTIME_SOURCE_SCHEMA_STATE_UPDATE_LOCK
        .lock()
        .expect("runtime source schema state update lock poisoned");
    let current = RUNTIME_SOURCE_SCHEMA_STATE.load();
    let mut namespaces = current.namespaces.clone();
    let mut effective_namespaces = metadata_schema_state_namespaces();
    for (namespace, output) in current.namespaces.iter() {
        effective_namespaces.insert(namespace.clone(), output.clone());
    }
    let mut changed_namespaces = Vec::new();
    for (namespace, output) in schema_state.namespaces {
        if effective_namespaces
            .get(&namespace)
            .is_some_and(|current| output_schema_equivalent(current, &output))
        {
            continue;
        }
        effective_namespaces.insert(namespace.clone(), output.clone());
        namespaces.insert(namespace.clone(), output);
        changed_namespaces.push(namespace);
    }
    let version = current.version.max(schema_state.version);
    RUNTIME_SOURCE_SCHEMA_STATE.store(Arc::new(RuntimeSchemaState {
        version,
        namespaces,
    }));
    let _ = PIPELINE_SCHEMA_VERSION.fetch_max(version, Ordering::AcqRel);
    changed_namespaces
}

pub fn current_runtime_schema_state() -> RuntimeSchemaState {
    let mut namespaces = metadata_schema_state_namespaces();
    let runtime_source_schema_state = RUNTIME_SOURCE_SCHEMA_STATE.load();
    for (namespace, output) in runtime_source_schema_state.namespaces.iter() {
        namespaces.insert(namespace.clone(), output.clone());
    }

    RuntimeSchemaState {
        version: current_pipeline_schema_version().max(runtime_source_schema_state.version),
        namespaces,
    }
}

pub fn runtime_schema_output_metadata(namespace: &str) -> Option<OutputMetadata> {
    current_runtime_schema_state()
        .namespaces
        .get(namespace)
        .cloned()
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    use serial_test::serial;

    use super::*;
    use crate::discover::{Metadata, PipelineMetadata, SkipprDataType};

    fn root_record_with_field(field_name: &str, field_type: SkipprDataType) -> Metadata {
        let mut root = Metadata::new_with_type(SkipprDataType::Record, "");
        root.fields.insert(
            field_name.to_string(),
            Metadata::new_with_type(field_type, field_name),
        );
        root
    }

    fn install_metadata(namespaces: HashMap<String, Metadata>) {
        let mut pipeline_metadata = PipelineMetadata::new();
        pipeline_metadata.metadata = namespaces;
        pipeline_metadata.flattened = false;
        METADATA.store(Arc::new(pipeline_metadata));
        PIPELINE_SCHEMA_VERSION.store(0, Ordering::Release);
        clear_runtime_source_schema_state();
    }

    fn with_metadata_test_lock<T>(f: impl FnOnce() -> T) -> T {
        let _guard = crate::metadata_test_lock();
        f()
    }

    #[test]
    #[serial]
    fn identical_runtime_schema_state_reports_no_changed_namespaces() {
        with_metadata_test_lock(|| {
            let namespace_metadata = root_record_with_field("id", SkipprDataType::String);
            let output = OutputMetadata::from_metadata(&namespace_metadata);
            install_metadata(HashMap::from([("events".to_string(), namespace_metadata)]));

            let changed = apply_runtime_source_schema_state(RuntimeSchemaState {
                version: 1,
                namespaces: BTreeMap::from([("events".to_string(), output)]),
            });

            assert!(changed.is_empty());
        });
    }

    #[test]
    #[serial]
    fn schema_state_change_detection_ignores_non_published_ids() {
        with_metadata_test_lock(|| {
            let namespace_metadata = root_record_with_field("id", SkipprDataType::String);
            let mut output = OutputMetadata::from_metadata(&namespace_metadata);
            output.field_id = 42;
            output.schema_id = 99;
            output.lineage_id = "runtime-lineage".to_string();
            install_metadata(HashMap::from([("events".to_string(), namespace_metadata)]));

            let changed = apply_runtime_source_schema_state(RuntimeSchemaState {
                version: 1,
                namespaces: BTreeMap::from([("events".to_string(), output)]),
            });

            assert!(changed.is_empty());
        });
    }

    #[test]
    #[serial]
    fn changed_runtime_schema_state_reports_only_changed_namespaces() {
        with_metadata_test_lock(|| {
            let events_metadata = root_record_with_field("id", SkipprDataType::String);
            let metadata_output = OutputMetadata::from_metadata(&events_metadata);
            let changed_output =
                OutputMetadata::from_metadata(&root_record_with_field("id", SkipprDataType::Long));
            install_metadata(HashMap::from([("events".to_string(), events_metadata)]));

            let changed = apply_runtime_source_schema_state(RuntimeSchemaState {
                version: 1,
                namespaces: BTreeMap::from([
                    ("events".to_string(), metadata_output),
                    ("users".to_string(), changed_output),
                ]),
            });

            assert_eq!(changed, vec!["users".to_string()]);
        });
    }
}
