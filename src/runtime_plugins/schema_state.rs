use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;

use arc_swap::ArcSwap;
use once_cell::sync::Lazy;

use crate::discover::{OutputMetadata, PipelineMetadata};
use crate::helpers::configuration::Config;
use crate::helpers::Helpers;
use crate::ingest_work::storage_namespace;
use crate::runtime_plugins::protocol::RuntimeSchemaState;
use crate::{METADATA, PIPELINE_SCHEMA_VERSION};

static RUNTIME_SOURCE_SCHEMA_STATE: Lazy<ArcSwap<RuntimeSchemaState>> = Lazy::new(|| {
    ArcSwap::new(Arc::new(RuntimeSchemaState {
        version: 0,
        namespaces: BTreeMap::new(),
        namespace_versions: BTreeMap::new(),
    }))
});
static RUNTIME_SOURCE_SCHEMA_STATE_UPDATE_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));
static RUNTIME_SOURCE_SCHEMA_NAMESPACE_VERSIONS: Lazy<Mutex<BTreeMap<String, u64>>> =
    Lazy::new(|| Mutex::new(BTreeMap::new()));

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
        namespace_versions: BTreeMap::new(),
    }));
    RUNTIME_SOURCE_SCHEMA_NAMESPACE_VERSIONS
        .lock()
        .expect("runtime source schema namespace versions lock poisoned")
        .clear();
}

pub(crate) fn transform_time_fields(config: &Config) -> Vec<String> {
    config
        .get_transform_batch_time_fields()
        .split(',')
        .map(str::trim)
        .filter(|field| !field.is_empty())
        .map(str::to_string)
        .collect()
}

fn metadata_schema_state_namespaces(
    config: &Config,
) -> Result<BTreeMap<String, OutputMetadata>, String> {
    let pipeline = METADATA.load();
    pipeline
        .metadata
        .keys()
        .map(|namespace| {
            let output =
                published_output_metadata(config, &pipeline, namespace)?.ok_or_else(|| {
                    format!("published dest schema is missing namespace '{namespace}'")
                })?;
            Ok((namespace.clone(), output))
        })
        .collect()
}

/// Published dest shape: fold `PipelineMetadata.alters`, then project, then
/// inject fields and the event-time type. Missing ns falls back to runtime
/// source state. Flatten-mode mismatch fails closed.
pub fn published_output_metadata(
    config: &Config,
    pipeline: &PipelineMetadata,
    namespace: &str,
) -> Result<Option<OutputMetadata>, String> {
    let output = match pipeline.output_metadata(namespace) {
        Ok(output) => Some(output),
        Err(crate::discover::schema_alter::SchemaAlterError::NamespaceNotFound(_)) => {
            runtime_schema_output_metadata(namespace)?
        }
        Err(err) => return Err(err.to_string()),
    };
    let Some(mut output) = output else {
        return Ok(None);
    };
    apply_published_dest(
        namespace,
        &mut output,
        &config.get_transform_inject_fields(),
        &transform_time_fields(config),
    )?;
    if let Some(select) = crate::compute::published_stateless_select(config, namespace)
        .map_err(|err| err.to_string())?
    {
        fold_stateless_derive(namespace, &mut output, &select)?;
    }
    Ok(Some(output))
}

/// Dest-only columns from Stateless derive. Source fields keep their ids.
fn fold_stateless_derive(
    namespace: &str,
    output: &mut OutputMetadata,
    select: &crate::compute::StatelessSelect,
) -> Result<(), String> {
    if select.derive.is_empty() {
        return Ok(());
    }
    let source = crate::converters::skippr_arrow::convert_skippr_to_arrow(output.fields.clone())
        .map_err(|err| err.to_string())?;
    let dest = crate::compute::dest_schema(&[], Some(select), std::sync::Arc::new(source))?;
    for field in dest.fields() {
        if output.fields.contains_key(field.name()) {
            continue;
        }
        let mut child = crate::ingest::schema_arrow::output_metadata_for_arrow_field(field)
            .map_err(|err| err.to_string())?;
        child.field_id = crate::lineage::deterministic_field_id(namespace, &[field.name().clone()]);
        output.fields.insert(field.name().clone(), child);
    }
    Ok(())
}

fn promotable_event_time(data_type: &crate::discover::SkipprDataType) -> bool {
    use crate::discover::SkipprDataType;
    matches!(
        data_type,
        SkipprDataType::Byte
            | SkipprDataType::Short
            | SkipprDataType::Integer
            | SkipprDataType::Long
            | SkipprDataType::Decimal
            | SkipprDataType::Date
            | SkipprDataType::Timestamp
            | SkipprDataType::TimestampMilli
            | SkipprDataType::String
    )
}

pub(crate) fn inject_json_data_type(
    name: &str,
    value: &serde_json::Value,
) -> Result<crate::discover::SkipprDataType, String> {
    use crate::discover::SkipprDataType;
    match value {
        serde_json::Value::String(_) => Ok(SkipprDataType::String),
        serde_json::Value::Number(_) => Ok(SkipprDataType::Double),
        serde_json::Value::Bool(_) => Ok(SkipprDataType::Boolean),
        other => Err(format!(
            "inject field '{name}' has unsupported JSON value {other}"
        )),
    }
}

pub(crate) fn apply_published_dest(
    namespace: &str,
    output: &mut OutputMetadata,
    inject: &std::collections::BTreeMap<String, serde_json::Value>,
    time_fields: &[String],
) -> Result<(), String> {
    use crate::discover::SkipprDataType;
    if crate::ingest::deadletter::is_deadletter_namespace(namespace) {
        return Ok(());
    }
    for (name, value) in inject {
        if output.fields.contains_key(name) {
            continue;
        }
        let data_type = inject_json_data_type(name, value)?;
        let mut field = OutputMetadata::new();
        field.out_field_name = name.clone();
        field.source_field_name = name.clone();
        field.determined_type = data_type;
        field.nullable = true;
        field.field_id = crate::lineage::deterministic_field_id(namespace, &[name.clone()]);
        output.fields.insert(name.clone(), field);
    }
    for configured in time_fields {
        let Some(name) = Helpers::field_name_candidates(configured)
            .into_iter()
            .find(|name| output.fields.contains_key(name))
        else {
            return Err(format!(
                "event time field '{configured}' is not in the published schema"
            ));
        };
        let field = output.fields.get_mut(&name).expect("checked");
        if !promotable_event_time(&field.determined_type) {
            return Err(format!(
                "event time field '{name}' type {} cannot be a timestamp",
                field.determined_type.as_str()
            ));
        }
        field.determined_type = SkipprDataType::Timestamp;
    }
    Ok(())
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

pub fn apply_runtime_source_schema_state(
    config: &Config,
    schema_state: RuntimeSchemaState,
) -> Result<Vec<String>, String> {
    let _guard = RUNTIME_SOURCE_SCHEMA_STATE_UPDATE_LOCK
        .lock()
        .expect("runtime source schema state update lock poisoned");
    let current = RUNTIME_SOURCE_SCHEMA_STATE.load();
    let mut namespaces = current.namespaces.clone();
    let mut namespace_versions = current.namespace_versions.clone();
    let dest_namespaces = metadata_schema_state_namespaces(config)?;
    for namespace in dest_namespaces.keys() {
        namespace_versions
            .entry(namespace.clone())
            .or_insert_with(|| crate::ingest_work::namespace_schema_version(namespace).max(1));
    }
    let mut changed_namespaces = Vec::new();
    let update_version = schema_state.version;
    for (namespace, output) in schema_state.namespaces {
        let namespace = storage_namespace(config, &namespace);
        let namespace_version = schema_state
            .namespace_versions
            .get(&namespace)
            .copied()
            .unwrap_or(update_version);
        if dest_namespaces.contains_key(&namespace) {
            continue;
        }
        if current
            .namespaces
            .get(&namespace)
            .is_some_and(|installed| output_schema_equivalent(installed, &output))
            && namespace_versions
                .get(&namespace)
                .is_some_and(|installed| *installed >= namespace_version)
        {
            continue;
        }
        namespaces.insert(namespace.clone(), output);
        namespace_versions.insert(namespace.clone(), namespace_version);
        RUNTIME_SOURCE_SCHEMA_NAMESPACE_VERSIONS
            .lock()
            .expect("runtime source schema namespace versions lock poisoned")
            .insert(namespace.clone(), namespace_version);
        changed_namespaces.push(namespace);
    }
    let version = current.version.max(update_version);
    RUNTIME_SOURCE_SCHEMA_STATE.store(Arc::new(RuntimeSchemaState {
        version,
        namespaces,
        namespace_versions,
    }));
    let _ = PIPELINE_SCHEMA_VERSION.fetch_max(version, Ordering::AcqRel);
    Ok(changed_namespaces)
}

pub fn runtime_schema_namespace_version(namespace: &str) -> u64 {
    RUNTIME_SOURCE_SCHEMA_NAMESPACE_VERSIONS
        .lock()
        .expect("runtime source schema namespace versions lock poisoned")
        .get(namespace)
        .copied()
        .unwrap_or(0)
}

pub fn current_runtime_schema_state(config: &Config) -> Result<RuntimeSchemaState, String> {
    let mut namespaces = metadata_schema_state_namespaces(config)?;
    let mut namespace_versions = namespaces
        .keys()
        .map(|namespace| {
            (
                namespace.clone(),
                crate::ingest_work::namespace_schema_version(namespace).max(1),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let runtime_source_schema_state = RUNTIME_SOURCE_SCHEMA_STATE.load();
    let pipeline = METADATA.load();
    for namespace in runtime_source_schema_state.namespaces.keys() {
        if namespaces.contains_key(namespace) {
            continue;
        }
        if let Some(output) = published_output_metadata(config, &pipeline, namespace)? {
            namespaces.insert(namespace.clone(), output);
        }
    }
    for (namespace, version) in runtime_source_schema_state.namespace_versions.iter() {
        if !namespaces.contains_key(namespace) {
            continue;
        }
        namespace_versions
            .entry(namespace.clone())
            .or_insert(*version);
    }

    Ok(RuntimeSchemaState {
        version: current_pipeline_schema_version().max(runtime_source_schema_state.version),
        namespaces,
        namespace_versions,
    })
}

pub fn runtime_schema_output_metadata(namespace: &str) -> Result<Option<OutputMetadata>, String> {
    Ok(RUNTIME_SOURCE_SCHEMA_STATE
        .load()
        .namespaces
        .get(namespace)
        .cloned())
}

pub fn install_from_pipeline_metadata(
    config: &Config,
    metadata: &crate::discover::PipelineMetadata,
) -> u64 {
    crate::METADATA.store(Arc::new(metadata.clone()));
    for namespace in metadata.metadata.keys() {
        if namespace.is_empty() {
            continue;
        }
        crate::ingest_work::Ingest::prepare_arrow_schema_with_metadata(config, namespace, metadata)
            .unwrap_or_else(|err| panic!("published dest Arrow schema for {namespace}: {err}"));
    }
    let namespaces = metadata_schema_state_namespaces(config).expect("published dest schema");
    let namespace_versions = namespaces
        .keys()
        .map(|namespace| {
            (
                namespace.clone(),
                crate::ingest_work::namespace_schema_version(namespace).max(1),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let version = namespace_versions.values().copied().max().unwrap_or(1);
    apply_runtime_source_schema_state(
        config,
        RuntimeSchemaState {
            version,
            namespaces,
            namespace_versions,
        },
    )
    .expect("published dest schema");
    version
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    use serial_test::serial;

    use super::*;
    use crate::discover::{Metadata, PipelineMetadata, SkipprDataType};
    use crate::helpers::configuration::Config;

    fn root_record_with_field(field_name: &str, field_type: SkipprDataType) -> Metadata {
        let mut root = Metadata::new_with_type(SkipprDataType::Record, "");
        root.fields.insert(
            field_name.to_string(),
            Metadata::new_with_type(field_type, field_name),
        );
        root
    }

    fn install_metadata(namespaces: HashMap<String, Metadata>) {
        let mut pipeline_metadata = PipelineMetadata::new(&Config::new());
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

            let changed = apply_runtime_source_schema_state(
                &Config::new(),
                RuntimeSchemaState {
                    version: 1,
                    namespaces: BTreeMap::from([("events".to_string(), output)]),
                    namespace_versions: BTreeMap::from([("events".to_string(), 1)]),
                },
            )
            .unwrap();

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

            let changed = apply_runtime_source_schema_state(
                &Config::new(),
                RuntimeSchemaState {
                    version: 1,
                    namespaces: BTreeMap::from([("events".to_string(), output)]),
                    namespace_versions: BTreeMap::from([("events".to_string(), 1)]),
                },
            )
            .unwrap();

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

            let changed = apply_runtime_source_schema_state(
                &Config::new(),
                RuntimeSchemaState {
                    version: 1,
                    namespaces: BTreeMap::from([
                        ("events".to_string(), metadata_output),
                        ("users".to_string(), changed_output),
                    ]),
                    namespace_versions: BTreeMap::from([
                        ("events".to_string(), 1),
                        ("users".to_string(), 1),
                    ]),
                },
            )
            .unwrap();

            assert_eq!(changed, vec!["users".to_string()]);
        });
    }

    #[test]
    #[serial]
    fn install_from_pipeline_metadata_publishes_runtime_schema_state() {
        with_metadata_test_lock(|| {
            let namespace_metadata = root_record_with_field("id", SkipprDataType::String);
            let mut pipeline_metadata = PipelineMetadata::new(&Config::new());
            pipeline_metadata.metadata =
                HashMap::from([("events".to_string(), namespace_metadata)]);
            pipeline_metadata.flattened = false;
            pipeline_metadata.metadata_version = 4;
            clear_runtime_source_schema_state();
            PIPELINE_SCHEMA_VERSION.store(0, Ordering::Release);
            let version = install_from_pipeline_metadata(&Config::new(), &pipeline_metadata);
            assert_eq!(
                version,
                crate::ingest_work::namespace_schema_version("events").max(1)
            );
            assert!(current_runtime_schema_state(&Config::new())
                .unwrap()
                .namespaces
                .contains_key("events"));
            assert!(crate::ARROW_SCHEMA.get("events").is_some());
        });
    }

    #[test]
    #[serial]
    fn dest_output_metadata_is_not_overwritten_by_source_schema_state() {
        use crate::discover::schema_alter::{
            AlterColumn, AlterOrigin, CanonicalAlter, CommittedAlter, FlattenMode,
            MigrationDirection, MigrationId, OutputName,
        };

        with_metadata_test_lock(|| {
            let mut root = Metadata::new().unwrap();
            let mut region = Metadata::new_with_type(SkipprDataType::String, "region");
            region.field_id = 1;
            root.fields.insert("region".into(), region);
            let nested = OutputName::parse_sql(FlattenMode::Nested, "region").unwrap();
            let mut pipeline = PipelineMetadata::new(&Config::new());
            pipeline.flattened = false;
            pipeline.metadata.insert("orders".into(), root.clone());
            pipeline.alters.insert(
                "orders".into(),
                vec![CommittedAlter {
                    origin: AlterOrigin {
                        migration: MigrationId::new("m"),
                        direction: MigrationDirection::Up,
                    },
                    seq: 0,
                    alter: CanonicalAlter::Rename {
                        column: AlterColumn {
                            name: nested,
                            id: 1,
                            ty: SkipprDataType::String,
                        },
                        to_leaf: "region_code".into(),
                    },
                }],
            );
            METADATA.store(Arc::new(pipeline));
            PIPELINE_SCHEMA_VERSION.store(0, Ordering::Release);
            clear_runtime_source_schema_state();

            let source = OutputMetadata::from_metadata(&root);
            let changed = apply_runtime_source_schema_state(
                &Config::new(),
                RuntimeSchemaState {
                    version: 1,
                    namespaces: BTreeMap::from([("orders".to_string(), source)]),
                    namespace_versions: BTreeMap::from([("orders".to_string(), 1)]),
                },
            )
            .unwrap();
            assert!(changed.is_empty());

            let published = current_runtime_schema_state(&Config::new()).unwrap();
            let names: Vec<_> = published.namespaces["orders"]
                .child_fields()
                .map(|(name, _)| name.as_str())
                .collect();
            assert!(names.contains(&"region_code"), "{names:?}");
            assert!(!names.contains(&"region"), "{names:?}");
        });
    }

    #[test]
    #[serial]
    fn runtime_source_only_namespace_uses_published_output_metadata() {
        with_metadata_test_lock(|| {
            METADATA.store(Arc::new(PipelineMetadata::new(&Config::new())));
            PIPELINE_SCHEMA_VERSION.store(0, Ordering::Release);
            clear_runtime_source_schema_state();
            let mut source = OutputMetadata::new();
            source.determined_type = SkipprDataType::Record;
            let mut region = OutputMetadata::new();
            region.out_field_name = "region".into();
            region.determined_type = SkipprDataType::String;
            source.fields.insert("region".into(), region);
            apply_runtime_source_schema_state(
                &Config::new(),
                RuntimeSchemaState {
                    version: 2,
                    namespaces: BTreeMap::from([("orders".into(), source)]),
                    namespace_versions: BTreeMap::from([("orders".into(), 2)]),
                },
            )
            .unwrap();
            let published = current_runtime_schema_state(&Config::new()).unwrap();
            assert!(published.namespaces.contains_key("orders"));
            assert_eq!(published.namespace_versions["orders"], 2);
            assert!(published.namespaces["orders"].fields.contains_key("region"));
        });
    }

    #[test]
    #[serial]
    fn runtime_source_version_without_metadata_is_not_published() {
        with_metadata_test_lock(|| {
            METADATA.store(Arc::new(PipelineMetadata::new(&Config::new())));
            PIPELINE_SCHEMA_VERSION.store(0, Ordering::Release);
            clear_runtime_source_schema_state();
            apply_runtime_source_schema_state(
                &Config::new(),
                RuntimeSchemaState {
                    version: 3,
                    namespaces: BTreeMap::new(),
                    namespace_versions: BTreeMap::from([("ghost".into(), 3)]),
                },
            )
            .unwrap();
            let published = current_runtime_schema_state(&Config::new()).unwrap();
            assert!(!published.namespaces.contains_key("ghost"));
            assert!(!published.namespace_versions.contains_key("ghost"));
        });
    }

    #[test]
    #[serial]
    fn published_output_metadata_fails_closed_on_flatten_mismatch() {
        use crate::discover::schema_alter::{
            AlterColumn, AlterOrigin, CanonicalAlter, CommittedAlter, FlattenMode,
            MigrationDirection, MigrationId, OutputName,
        };

        with_metadata_test_lock(|| {
            let mut root = Metadata::new().unwrap();
            let mut region = Metadata::new_with_type(SkipprDataType::String, "region");
            region.field_id = 1;
            root.fields.insert("region".into(), region);
            let nested = OutputName::parse_sql(FlattenMode::Nested, "region").unwrap();
            let mut pipeline = PipelineMetadata::new(&Config::new());
            pipeline.flattened = true;
            pipeline.metadata.insert("orders".into(), root);
            pipeline.alters.insert(
                "orders".into(),
                vec![CommittedAlter {
                    origin: AlterOrigin {
                        migration: MigrationId::new("m"),
                        direction: MigrationDirection::Up,
                    },
                    seq: 0,
                    alter: CanonicalAlter::Rename {
                        column: AlterColumn {
                            name: nested,
                            id: 1,
                            ty: SkipprDataType::String,
                        },
                        to_leaf: "region_code".into(),
                    },
                }],
            );
            METADATA.store(Arc::new(pipeline));
            PIPELINE_SCHEMA_VERSION.store(0, Ordering::Release);
            clear_runtime_source_schema_state();
            let err =
                published_output_metadata(&Config::new(), &METADATA.load(), "orders").unwrap_err();
            assert!(
                err.to_lowercase().contains("flatten") || err.contains("Flatten"),
                "{err}"
            );
        });
    }

    #[test]
    fn install_from_pipeline_metadata_does_not_discard_dest_fold() {
        let src = include_str!("schema_state.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        assert!(
            !prod
                .contains("let _ = crate::ingest_work::Ingest::prepare_arrow_schema_with_metadata"),
            "install must not discard dest fold"
        );
        assert!(
            prod.contains("unwrap_or_else(|err| panic!(\"published dest Arrow schema for {namespace}: {err}\"))"),
            "install must fail closed on dest fold"
        );
    }

    #[test]
    fn event_time_string_becomes_timestamp_and_float_fails() {
        let mut output = OutputMetadata::new();
        output.determined_type = SkipprDataType::Record;
        let mut event = OutputMetadata::new();
        event.out_field_name = "event_ts".into();
        event.determined_type = SkipprDataType::String;
        event.field_id = 2;
        output.fields.insert("event_ts".into(), event);
        apply_published_dest(
            "orders",
            &mut output,
            &BTreeMap::new(),
            &["event_ts".into()],
        )
        .unwrap();
        assert_eq!(
            output.fields["event_ts"].determined_type,
            SkipprDataType::Timestamp
        );

        output.fields.get_mut("event_ts").unwrap().determined_type = SkipprDataType::Float;
        let err = apply_published_dest(
            "orders",
            &mut output,
            &BTreeMap::new(),
            &["event_ts".into()],
        )
        .unwrap_err();
        assert!(err.contains("cannot be a timestamp"), "{err}");

        output.fields.get_mut("event_ts").unwrap().determined_type = SkipprDataType::Boolean;
        let err = apply_published_dest(
            "orders",
            &mut output,
            &BTreeMap::new(),
            &["event_ts".into()],
        )
        .unwrap_err();
        assert!(err.contains("cannot be a timestamp"), "{err}");

        output.fields.get_mut("event_ts").unwrap().determined_type = SkipprDataType::Long;
        apply_published_dest(
            "orders",
            &mut output,
            &BTreeMap::new(),
            &["event_ts".into()],
        )
        .unwrap();
        assert_eq!(
            output.fields["event_ts"].determined_type,
            SkipprDataType::Timestamp
        );

        let err = apply_published_dest(
            "orders",
            &mut output,
            &BTreeMap::new(),
            &["missing_ts".into()],
        )
        .unwrap_err();
        assert!(err.contains("is not in the published schema"), "{err}");
    }

    #[test]
    fn inject_field_is_added_and_object_values_fail() {
        let mut output = OutputMetadata::new();
        output.determined_type = SkipprDataType::Record;
        let mut inject = BTreeMap::new();
        inject.insert("source".into(), serde_json::json!("cube"));
        inject.insert("score".into(), serde_json::json!(1.5));
        inject.insert("ok".into(), serde_json::json!(true));
        apply_published_dest("orders", &mut output, &inject, &[]).unwrap();
        assert_eq!(
            output.fields["source"].determined_type,
            SkipprDataType::String
        );
        assert_eq!(
            output.fields["score"].determined_type,
            SkipprDataType::Double
        );
        assert_eq!(output.fields["ok"].determined_type, SkipprDataType::Boolean);
        assert!(output.fields["source"].field_id >= 1);

        inject.insert("bad".into(), serde_json::json!({"a": 1}));
        let err = apply_published_dest("orders", &mut output, &inject, &[]).unwrap_err();
        assert!(err.contains("inject field 'bad'"), "{err}");
    }

    #[test]
    fn deadletter_dest_does_not_take_inject_or_event_time() {
        let mut output = OutputMetadata::new();
        output.determined_type = SkipprDataType::Record;
        let mut event = OutputMetadata::new();
        event.out_field_name = "event_time".into();
        event.determined_type = SkipprDataType::Long;
        output.fields.insert("event_time".into(), event);
        let mut inject = BTreeMap::new();
        inject.insert("source".into(), serde_json::json!("cube"));
        apply_published_dest("_dl_orders", &mut output, &inject, &["event_time".into()]).unwrap();
        assert!(!output.fields.contains_key("source"));
        assert_eq!(
            output.fields["event_time"].determined_type,
            SkipprDataType::Long
        );
    }

    #[test]
    #[serial]
    fn published_dest_folds_stateless_derive() {
        let src = include_str!("schema_state.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        assert!(
            prod.contains("fold_stateless_derive"),
            "published dest must fold Stateless derive"
        );
        assert!(
            prod.contains("published_stateless_select"),
            "published dest must read the compute journal"
        );
    }

    #[test]
    fn dest_schema_includes_stateless_derive_column() {
        let mut output = OutputMetadata::new();
        output.determined_type = SkipprDataType::Record;
        let mut amount = OutputMetadata::new();
        amount.out_field_name = "amount".into();
        amount.determined_type = SkipprDataType::Long;
        amount.field_id = 2;
        output.fields.insert("amount".into(), amount);
        let select = crate::compute::StatelessSelect {
            filter: None,
            derive: vec![("amount_gbp".into(), "amount * 0.8".into())],
        };
        fold_stateless_derive("orders", &mut output, &select).unwrap();
        assert!(
            output.fields.contains_key("amount_gbp"),
            "SQL compose dest must publish amount_gbp"
        );
        assert_eq!(output.fields["amount"].field_id, 2);
        assert!(output.fields["amount_gbp"].field_id >= 1);
    }

    #[test]
    #[serial]
    fn published_output_metadata_is_none_when_namespace_is_missing() {
        with_metadata_test_lock(|| {
            METADATA.store(Arc::new(PipelineMetadata::new(&Config::new())));
            clear_runtime_source_schema_state();
            assert!(
                published_output_metadata(&Config::new(), &METADATA.load(), "orders")
                    .unwrap()
                    .is_none()
            );
        });
    }

    #[test]
    fn field_name_candidates_include_stripped_then_flattened() {
        let names = Helpers::field_name_candidates("_detail.region_");
        assert!(names.contains(&"_detail.region_".to_string()));
        assert!(names.contains(&"_detail_region_".to_string()));
        assert!(names.contains(&"detail.region".to_string()));
        assert!(names.contains(&"detail_region".to_string()));
    }
}
