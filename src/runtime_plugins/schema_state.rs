use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

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

pub fn current_pipeline_schema_version() -> u64 {
    PIPELINE_SCHEMA_VERSION.load(Ordering::Acquire)
}

pub fn bump_pipeline_schema_version() -> u64 {
    PIPELINE_SCHEMA_VERSION.fetch_add(1, Ordering::AcqRel) + 1
}

pub fn clear_runtime_source_schema_state() {
    RUNTIME_SOURCE_SCHEMA_STATE.store(Arc::new(RuntimeSchemaState {
        version: 0,
        namespaces: BTreeMap::new(),
    }));
}

pub fn apply_runtime_source_schema_state(schema_state: RuntimeSchemaState) {
    let version = schema_state.version;
    RUNTIME_SOURCE_SCHEMA_STATE.store(Arc::new(schema_state));
    let _ = PIPELINE_SCHEMA_VERSION.fetch_max(version, Ordering::AcqRel);
}

pub fn current_runtime_schema_state() -> RuntimeSchemaState {
    let metadata = METADATA.load();
    let mut namespaces = metadata
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
        .collect::<BTreeMap<_, _>>();
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
