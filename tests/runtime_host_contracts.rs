use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::task::{Context, Poll};

use arrow::array::{Int64Array, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use datafusion::error::DataFusionError;
use datafusion::physical_plan::{RecordBatchStream, SendableRecordBatchStream};
use futures::Stream;
use serde_json::json;
use serial_test::serial;
use sha2::{Digest, Sha256};
use skipprd::discover::OutputMetadata;
use skipprd::helpers::configuration::Config;
use skipprd::plugins::{DataSink, SchemaSink};
use skipprd::runtime_plugins::host::{
    ResolvedRuntimePlugin, RuntimeDataSinkPlugin, RuntimeSchemaSinkPlugin,
};
use skipprd::runtime_plugins::manifest::RuntimePluginManifest;
use skipprd::runtime_plugins::protocol::{
    RuntimeBinding, RuntimePluginConfigEnvelope, RuntimeSchemaConfig, RuntimeSinkConfig,
    RUNTIME_PROTOCOL_VERSION,
};
use tempfile::tempdir;

fn helper_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_skippr-runtime-plugin-test-helper"))
}

fn helper_sha256() -> String {
    let bytes = std::fs::read(helper_binary()).expect("failed to read helper binary");
    format!("{:x}", Sha256::digest(bytes))
}

fn sink_capability_json(name: &str) -> serde_json::Value {
    match name {
        "Postgres" => json!({
            "name": "Postgres",
            "max_sessions_per_child": 1,
            "guarantee_tier": "ExactOnceCdcEligible",
            "can_manage_skippr_columns": true,
            "can_maintain_tombstone_tables": true,
            "can_compare_order_tokens": true,
            "supports_transactions": true,
            "supports_bounded_grouped_stream": true,
            "retry_semantics": "FinalStateIdempotent",
            "grouping_support": "FinalStateBatches"
        }),
        _ => json!({
            "name": "File",
            "max_sessions_per_child": 4,
            "guarantee_tier": "CdcEncodedOnly",
            "can_manage_skippr_columns": false,
            "can_maintain_tombstone_tables": false,
            "can_compare_order_tokens": false,
            "supports_transactions": false,
            "supports_bounded_grouped_stream": true,
            "retry_semantics": "DeterministicOverwrite",
            "grouping_support": "CdcEncodedBatches"
        }),
    }
}

fn write_sink_manifest(
    dir: &Path,
    name: &str,
    plugin_name: &str,
    scenario: &str,
    sink_capability_name: &str,
    sha256: Option<&str>,
    executable: &Path,
    marker_path: Option<&Path>,
) -> PathBuf {
    let args = if let Some(marker_path) = marker_path {
        json!([
            "--kind",
            "data_sink",
            "--plugin-name",
            plugin_name,
            "--scenario",
            scenario,
            "--marker-path",
            marker_path.display().to_string()
        ])
    } else {
        json!([
            "--kind",
            "data_sink",
            "--plugin-name",
            plugin_name,
            "--scenario",
            scenario
        ])
    };

    let mut artifacts = serde_json::Map::new();
    artifacts.insert(
        RuntimePluginManifest::current_target(),
        json!({
            "executable": executable.display().to_string(),
            "sha256": sha256
        }),
    );

    let manifest_path = dir.join(format!("{}.json", name));
    let manifest = json!({
        "name": name,
        "kind": "DataSink",
        "plugin_name": plugin_name,
        "version": "test",
        "protocol_version": RUNTIME_PROTOCOL_VERSION,
        "config_schema_version": 1,
        "artifacts": artifacts,
        "args": args,
        "supports_schema": false,
        "sink_capability": sink_capability_json(sink_capability_name)
    });
    std::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    manifest_path
}

fn write_schema_manifest(
    dir: &Path,
    name: &str,
    plugin_name: &str,
    scenario: &str,
    sha256: Option<&str>,
    executable: &Path,
    marker_path: Option<&Path>,
) -> PathBuf {
    let args = if let Some(marker_path) = marker_path {
        json!([
            "--kind",
            "schema_sink",
            "--plugin-name",
            plugin_name,
            "--scenario",
            scenario,
            "--marker-path",
            marker_path.display().to_string()
        ])
    } else {
        json!([
            "--kind",
            "schema_sink",
            "--plugin-name",
            plugin_name,
            "--scenario",
            scenario
        ])
    };

    let mut artifacts = serde_json::Map::new();
    artifacts.insert(
        RuntimePluginManifest::current_target(),
        json!({
            "executable": executable.display().to_string(),
            "sha256": sha256
        }),
    );

    let manifest_path = dir.join(format!("{}.json", name));
    let manifest = json!({
        "name": name,
        "kind": "SchemaSink",
        "plugin_name": plugin_name,
        "version": "test",
        "protocol_version": RUNTIME_PROTOCOL_VERSION,
        "config_schema_version": 1,
        "artifacts": artifacts,
        "args": args,
        "supports_schema": false
    });
    std::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    manifest_path
}

fn runtime_file_sink_config() -> RuntimeSinkConfig {
    RuntimeSinkConfig(RuntimePluginConfigEnvelope::new(
        "File",
        json!({
            "format": "parquet",
            "output_dir": null
        }),
    ))
}

fn runtime_glue_schema_config() -> RuntimeSchemaConfig {
    RuntimeSchemaConfig(RuntimePluginConfigEnvelope::new(
        "Glue",
        json!({
            "glue_database_name": "runtime_host_contracts"
        }),
    ))
}

fn sample_output_metadata() -> OutputMetadata {
    serde_json::from_value(json!({
        "out_field_name": "",
        "determined_type": "record",
        "determined_type_values": "",
        "fields": {
            "id": {
                "out_field_name": "id",
                "determined_type": "string",
                "determined_type_values": "",
                "fields": {}
            }
        }
    }))
    .expect("sample output metadata should deserialize")
}

fn json_u64_array(value: &serde_json::Value, key: &str) -> Vec<u64> {
    value[key]
        .as_array()
        .unwrap_or_else(|| panic!("{key} should be an array"))
        .iter()
        .map(|entry| {
            entry
                .as_u64()
                .unwrap_or_else(|| panic!("{key} entries should be u64"))
        })
        .collect()
}

fn json_string_array(value: &serde_json::Value, key: &str) -> Vec<String> {
    value[key]
        .as_array()
        .unwrap_or_else(|| panic!("{key} should be an array"))
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .unwrap_or_else(|| panic!("{key} entries should be strings"))
                .to_string()
        })
        .collect()
}

fn schema_install_count(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .count()
}

fn multiplex_process_count(marker_path: &Path) -> usize {
    let mut process_ids = std::fs::read_to_string(marker_path.with_extension("processes"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect::<Vec<_>>();
    process_ids.sort();
    process_ids.dedup();
    process_ids.len()
}

struct RuntimeSinkPoolTargetGuard {
    previous: usize,
}

struct RuntimeEnvGuard {
    key: &'static str,
    previous: Option<String>,
}

impl RuntimeEnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var(key).ok();
        Config::setenv(key, value);
        Config::reset_envcache();
        Self { key, previous }
    }
}

impl Drop for RuntimeEnvGuard {
    fn drop(&mut self) {
        if let Some(value) = self.previous.as_deref() {
            Config::setenv(self.key, value);
        } else {
            std::env::remove_var(self.key);
            Config::set_evncache(self.key, "");
        }
        Config::reset_envcache();
    }
}

impl RuntimeSinkPoolTargetGuard {
    fn set(target: usize) -> Self {
        skipprd::ingest::tuner::apply_env_caps();
        let previous =
            skipprd::metrics::counters::RUNTIME_SINK_POOL_TARGET.swap(target, Ordering::SeqCst);
        Self { previous }
    }
}

impl Drop for RuntimeSinkPoolTargetGuard {
    fn drop(&mut self) {
        skipprd::metrics::counters::RUNTIME_SINK_POOL_TARGET.store(self.previous, Ordering::SeqCst);
    }
}

struct SingleBatchStream {
    schema: Arc<Schema>,
    batch: Option<RecordBatch>,
}

impl Stream for SingleBatchStream {
    type Item = Result<RecordBatch, DataFusionError>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        Poll::Ready(this.batch.take().map(Ok))
    }
}

impl RecordBatchStream for SingleBatchStream {
    fn schema(&self) -> Arc<Schema> {
        self.schema.clone()
    }
}

fn sample_stream() -> SendableRecordBatchStream {
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("name", DataType::Utf8, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(Int64Array::from(vec![1])) as _,
            Arc::new(StringArray::from(vec!["alice"])) as _,
        ],
    )
    .unwrap();
    Box::pin(SingleBatchStream {
        schema,
        batch: Some(batch),
    })
}

struct DelayedBatchStream {
    schema: Arc<Schema>,
    delay: Pin<Box<tokio::time::Sleep>>,
    batch: Option<RecordBatch>,
}

impl Stream for DelayedBatchStream {
    type Item = Result<RecordBatch, DataFusionError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.delay.as_mut().poll(cx).is_pending() {
            return Poll::Pending;
        }
        Poll::Ready(this.batch.take().map(Ok))
    }
}

impl RecordBatchStream for DelayedBatchStream {
    fn schema(&self) -> Arc<Schema> {
        Arc::clone(&self.schema)
    }
}

fn delayed_sample_stream(delay: std::time::Duration) -> SendableRecordBatchStream {
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("name", DataType::Utf8, false),
    ]));
    let batch = RecordBatch::try_new(
        Arc::clone(&schema),
        vec![
            Arc::new(Int64Array::from(vec![1])) as _,
            Arc::new(StringArray::from(vec!["alice"])) as _,
        ],
    )
    .unwrap();
    Box::pin(DelayedBatchStream {
        schema,
        delay: Box::pin(tokio::time::sleep(delay)),
        batch: Some(batch),
    })
}

struct PanicOnPollStream {
    schema: Arc<Schema>,
}

impl Stream for PanicOnPollStream {
    type Item = Result<RecordBatch, DataFusionError>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        panic!("sink payload stream was consumed before PrepareAck::Ready")
    }
}

impl RecordBatchStream for PanicOnPollStream {
    fn schema(&self) -> Arc<Schema> {
        self.schema.clone()
    }
}

fn panic_on_poll_stream() -> SendableRecordBatchStream {
    Box::pin(PanicOnPollStream {
        schema: Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)])),
    })
}

async fn multiplex_test_sink(
    dir: &Path,
    scenario: &str,
    marker_path: &Path,
    pipeline_name: &str,
) -> RuntimeDataSinkPlugin {
    let manifest_path = write_sink_manifest(
        dir,
        &format!("{scenario}-runtime-sink"),
        "File",
        scenario,
        "File",
        Some(&helper_sha256()),
        &helper_binary(),
        Some(marker_path),
    );
    RuntimeDataSinkPlugin::new(
        ResolvedRuntimePlugin::load(&manifest_path).unwrap(),
        pipeline_name.to_string(),
        RuntimeBinding::Primary,
        runtime_file_sink_config(),
    )
    .await
    .unwrap()
}

#[tokio::test]
#[serial]
async fn runtime_sink_does_not_reinstall_unchanged_schema_between_writes() {
    let _pool_target = RuntimeSinkPoolTargetGuard::set(1);
    let temp = tempdir().unwrap();
    let marker_path = temp.path().join("schema-installs.log");
    let manifest_path = write_sink_manifest(
        temp.path(),
        "schema-publish-once-runtime-sink",
        "File",
        "record_schema_installs",
        "File",
        Some(&helper_sha256()),
        &helper_binary(),
        Some(&marker_path),
    );
    let sink = RuntimeDataSinkPlugin::new(
        ResolvedRuntimePlugin::load(&manifest_path).unwrap(),
        "runtime_host_schema_publish_once".to_string(),
        RuntimeBinding::Primary,
        runtime_file_sink_config(),
    )
    .await
    .unwrap();
    let startup_installs = schema_install_count(&marker_path);
    assert_eq!(startup_installs, 1);

    sink.sync(sample_stream(), "unchanged-schema-1".to_string(), None)
        .await
        .unwrap();
    sink.sync(sample_stream(), "unchanged-schema-2".to_string(), None)
        .await
        .unwrap();

    assert_eq!(schema_install_count(&marker_path), startup_installs);
}

#[tokio::test]
#[serial]
async fn runtime_sink_pool_growth_installs_latest_schema_once_on_new_worker() {
    let _pool_target = RuntimeSinkPoolTargetGuard::set(1);
    let temp = tempdir().unwrap();
    let marker_path = temp.path().join("schema-installs.log");
    let manifest_path = write_sink_manifest(
        temp.path(),
        "schema-pool-growth-runtime-sink",
        "File",
        "record_schema_installs",
        "File",
        Some(&helper_sha256()),
        &helper_binary(),
        Some(&marker_path),
    );
    let sink = RuntimeDataSinkPlugin::new(
        ResolvedRuntimePlugin::load(&manifest_path).unwrap(),
        "runtime_host_schema_pool_growth".to_string(),
        RuntimeBinding::Primary,
        runtime_file_sink_config(),
    )
    .await
    .unwrap();
    let startup_installs = schema_install_count(&marker_path);
    assert_eq!(startup_installs, 1);
    skipprd::metrics::counters::RUNTIME_SINK_POOL_TARGET.store(4, Ordering::SeqCst);

    let (first, second, third) = tokio::join!(
        sink.sync(sample_stream(), "pool-growth-1".to_string(), None),
        sink.sync(sample_stream(), "pool-growth-2".to_string(), None),
        sink.sync(sample_stream(), "pool-growth-3".to_string(), None),
    );
    first.unwrap();
    second.unwrap();
    third.unwrap();
    assert_eq!(
        schema_install_count(&marker_path),
        startup_installs + 1,
        "only the new worker should receive the latest snapshot"
    );

    sink.sync(sample_stream(), "pool-growth-4".to_string(), None)
        .await
        .unwrap();
    assert_eq!(schema_install_count(&marker_path), startup_installs + 1);
}

#[tokio::test]
async fn runtime_sink_restarts_after_child_crash() {
    let temp = tempdir().unwrap();
    let marker_path = temp.path().join("crash-once.marker");
    let manifest_path = write_sink_manifest(
        temp.path(),
        "restartable-runtime-sink",
        "File",
        "crash_once",
        "File",
        Some(&helper_sha256()),
        &helper_binary(),
        Some(&marker_path),
    );

    let resolved = ResolvedRuntimePlugin::load(&manifest_path).unwrap();
    let sink = RuntimeDataSinkPlugin::new(
        resolved,
        "runtime_host_restart".to_string(),
        RuntimeBinding::Primary,
        runtime_file_sink_config(),
    )
    .await
    .unwrap();

    sink.sync(sample_stream(), "restart-test".to_string(), None)
        .await
        .unwrap();

    assert!(
        marker_path.exists(),
        "helper should have crashed once before restart"
    );
}

#[tokio::test]
async fn prepare_already_applied_sends_zero_payload_bytes() {
    let temp = tempdir().unwrap();
    let marker_path = temp.path().join("already-applied-state.json");
    let manifest_path = write_sink_manifest(
        temp.path(),
        "already-applied-runtime-sink",
        "File",
        "already_applied",
        "File",
        Some(&helper_sha256()),
        &helper_binary(),
        Some(&marker_path),
    );
    let sink = RuntimeDataSinkPlugin::new(
        ResolvedRuntimePlugin::load(&manifest_path).unwrap(),
        "runtime_host_already_applied".to_string(),
        RuntimeBinding::Primary,
        runtime_file_sink_config(),
    )
    .await
    .unwrap();

    sink.sync(
        panic_on_poll_stream(),
        "already-applied-c=receipt-1".to_string(),
        None,
    )
    .await
    .unwrap();

    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(marker_path).unwrap()).unwrap();
    assert_eq!(state["run_count"], 0);
    assert_eq!(state["payload_bytes"], 0);
    assert_eq!(state["compaction_ids"][0], "receipt-1");
}

#[tokio::test]
#[serial]
async fn authoritative_preflight_persists_catalog_intent_without_payload() {
    let temp = tempdir().unwrap();
    let marker_path = temp.path().join("already-applied-intent-state.json");
    let manifest_path = write_sink_manifest(
        temp.path(),
        "already-applied-intent-runtime-sink",
        "File",
        "already_applied_with_intent",
        "File",
        Some(&helper_sha256()),
        &helper_binary(),
        Some(&marker_path),
    );
    let sink = RuntimeDataSinkPlugin::new_with_data_dir_for_test(
        ResolvedRuntimePlugin::load(&manifest_path).unwrap(),
        "runtime_host_already_applied_intent".to_string(),
        RuntimeBinding::Primary,
        runtime_file_sink_config(),
        temp.path().to_string_lossy().into_owned(),
    )
    .await
    .unwrap();

    sink.sync(
        panic_on_poll_stream(),
        "already-applied-intent-c=receipt-1".to_string(),
        None,
    )
    .await
    .unwrap();

    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(marker_path).unwrap()).unwrap();
    assert_eq!(state["run_count"], 0);
    assert_eq!(state["payload_bytes"], 0);
    let outbox = skipprd::catalog_outbox::CatalogOutbox::open(temp.path()).unwrap();
    let pending = outbox.scan_pending(10).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].intent.identity.namespace, "events");
}

#[tokio::test]
#[serial]
async fn preflight_receipt_replay_repairs_failed_outbox_persist_without_payload() {
    let temp = tempdir().unwrap();
    let marker_path = temp.path().join("repair-outbox-state.json");
    let manifest_path = write_sink_manifest(
        temp.path(),
        "repair-outbox-runtime-sink",
        "File",
        "already_applied_with_intent",
        "File",
        Some(&helper_sha256()),
        &helper_binary(),
        Some(&marker_path),
    );
    let sink = RuntimeDataSinkPlugin::new_with_data_dir_for_test(
        ResolvedRuntimePlugin::load(&manifest_path).unwrap(),
        "runtime_host_repair_outbox".to_string(),
        RuntimeBinding::Primary,
        runtime_file_sink_config(),
        temp.path().to_string_lossy().into_owned(),
    )
    .await
    .unwrap();
    let pending_dir = temp.path().join("segment_buffer/catalog_outbox/v1/pending");
    std::fs::remove_dir_all(&pending_dir).unwrap();
    std::fs::write(&pending_dir, b"force persist failure").unwrap();

    let first = sink
        .sync(
            panic_on_poll_stream(),
            "repair-outbox-c=receipt-1".to_string(),
            None,
        )
        .await;
    assert!(
        first.is_err(),
        "slice completion must fail before intent persistence"
    );

    std::fs::remove_file(&pending_dir).unwrap();
    std::fs::create_dir_all(&pending_dir).unwrap();
    sink.sync(
        panic_on_poll_stream(),
        "repair-outbox-c=receipt-1".to_string(),
        None,
    )
    .await
    .unwrap();

    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(marker_path).unwrap()).unwrap();
    assert_eq!(state["payload_bytes"], 0);
    let outbox = skipprd::catalog_outbox::CatalogOutbox::open(temp.path()).unwrap();
    assert_eq!(outbox.scan_pending(10).unwrap().len(), 1);
}

#[tokio::test]
#[serial]
async fn two_sink_sessions_interleave_on_one_runtime_child() {
    let _target = RuntimeSinkPoolTargetGuard::set(2);
    let _pool_size = RuntimeEnvGuard::set("RUNTIME_SINK_CONNECTION_POOL_SIZE", "1");
    let _sessions = RuntimeEnvGuard::set("RUNTIME_SINK_SESSIONS_PER_CHILD", "2");
    let _budget = RuntimeEnvGuard::set("RUNTIME_SINK_SESSION_BUDGET", "2");
    let _chunks = RuntimeEnvGuard::set("RUNTIME_SINK_PAYLOAD_CHUNK_BYTES", "64");
    let temp = tempdir().unwrap();
    let marker_path = temp.path().join("multiplex-interleave.json");
    let sink = multiplex_test_sink(
        temp.path(),
        "multiplex_delay",
        &marker_path,
        "runtime_host_multiplex_interleave",
    )
    .await;

    let delay = std::time::Duration::from_millis(25);
    let (first, second) = tokio::join!(
        sink.sync(
            delayed_sample_stream(delay),
            "interleave-a".to_string(),
            None
        ),
        sink.sync(
            delayed_sample_stream(delay),
            "interleave-b".to_string(),
            None
        ),
    );
    first.unwrap();
    second.unwrap();

    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(marker_path).unwrap()).unwrap();
    assert_eq!(state["max_active"], 2);
    let frames = json_u64_array(&state, "data_frame_request_ids");
    let first_id = frames[0];
    let other_at = frames
        .iter()
        .position(|request_id| *request_id != first_id)
        .expect("both requests should reach the shared data connection");
    assert!(
        frames[other_at + 1..]
            .iter()
            .any(|request_id| *request_id == first_id),
        "sink chunks should interleave across request ids: {frames:?}"
    );
}

#[tokio::test]
#[serial]
async fn per_child_session_capacity_bounds_in_flight_applies() {
    let _target = RuntimeSinkPoolTargetGuard::set(2);
    let _pool_size = RuntimeEnvGuard::set("RUNTIME_SINK_CONNECTION_POOL_SIZE", "1");
    let _sessions = RuntimeEnvGuard::set("RUNTIME_SINK_SESSIONS_PER_CHILD", "2");
    let _budget = RuntimeEnvGuard::set("RUNTIME_SINK_SESSION_BUDGET", "2");
    let temp = tempdir().unwrap();
    let marker_path = temp.path().join("multiplex-capacity.json");
    let sink = multiplex_test_sink(
        temp.path(),
        "multiplex_delay",
        &marker_path,
        "runtime_host_multiplex_capacity",
    )
    .await;

    let (first, second, third) = tokio::join!(
        sink.sync(sample_stream(), "capacity-a".to_string(), None),
        sink.sync(sample_stream(), "capacity-b".to_string(), None),
        sink.sync(sample_stream(), "capacity-c".to_string(), None),
    );
    first.unwrap();
    second.unwrap();
    third.unwrap();

    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(marker_path).unwrap()).unwrap();
    assert_eq!(state["max_active"], 2);
    assert_eq!(state["run_count"], 3);
}

#[tokio::test]
#[serial]
async fn session_target_uses_ceiling_process_count_without_eager_growth() {
    let _target = RuntimeSinkPoolTargetGuard::set(5);
    let _process_cap = RuntimeEnvGuard::set("RUNTIME_SINK_CONNECTION_POOL_SIZE", "16");
    let _sessions = RuntimeEnvGuard::set("RUNTIME_SINK_SESSIONS_PER_CHILD", "2");
    let _budget = RuntimeEnvGuard::set("RUNTIME_SINK_SESSION_BUDGET", "5");
    let temp = tempdir().unwrap();
    let marker_path = temp.path().join("multiplex-process-demand.json");
    let sink = multiplex_test_sink(
        temp.path(),
        "multiplex_process_hold",
        &marker_path,
        "runtime_host_session_demand",
    )
    .await;
    assert_eq!(sink.worker_count_for_test(), 1);

    let (one, two, three, four, five) = tokio::join!(
        sink.sync(sample_stream(), "demand-1".to_string(), None),
        sink.sync(sample_stream(), "demand-2".to_string(), None),
        sink.sync(sample_stream(), "demand-3".to_string(), None),
        sink.sync(sample_stream(), "demand-4".to_string(), None),
        sink.sync(sample_stream(), "demand-5".to_string(), None),
    );
    one.unwrap();
    two.unwrap();
    three.unwrap();
    four.unwrap();
    five.unwrap();

    assert_eq!(sink.worker_count_for_test(), 3);
    assert_eq!(multiplex_process_count(&marker_path), 3);
}

#[tokio::test]
#[serial]
async fn adapter_capability_limits_effective_per_child_sessions() {
    let _target = RuntimeSinkPoolTargetGuard::set(8);
    let _process_cap = RuntimeEnvGuard::set("RUNTIME_SINK_CONNECTION_POOL_SIZE", "16");
    let _sessions = RuntimeEnvGuard::set("RUNTIME_SINK_SESSIONS_PER_CHILD", "8");
    let _budget = RuntimeEnvGuard::set("RUNTIME_SINK_SESSION_BUDGET", "8");
    let temp = tempdir().unwrap();
    let file_marker = temp.path().join("file-capacity.json");
    let file_sink = multiplex_test_sink(
        temp.path(),
        "multiplex_delay",
        &file_marker,
        "runtime_host_file_adapter_limit",
    )
    .await;
    assert_eq!(file_sink.session_capacity_for_test(), 4);

    let postgres_marker = temp.path().join("postgres-capacity.json");
    let postgres_manifest = write_sink_manifest(
        temp.path(),
        "postgres-session-limit-runtime-sink",
        "Postgres",
        "multiplex_delay",
        "Postgres",
        Some(&helper_sha256()),
        &helper_binary(),
        Some(&postgres_marker),
    );
    let postgres_sink = RuntimeDataSinkPlugin::new(
        ResolvedRuntimePlugin::load(&postgres_manifest).unwrap(),
        "runtime_host_postgres_adapter_limit".to_string(),
        RuntimeBinding::Primary,
        runtime_file_sink_config(),
    )
    .await
    .unwrap();
    assert_eq!(postgres_sink.session_capacity_for_test(), 1);
}

#[tokio::test]
#[serial]
async fn runtime_sink_pool_shrinks_only_fully_idle_workers() {
    let _target = RuntimeSinkPoolTargetGuard::set(6);
    let _process_cap = RuntimeEnvGuard::set("RUNTIME_SINK_CONNECTION_POOL_SIZE", "16");
    let _sessions = RuntimeEnvGuard::set("RUNTIME_SINK_SESSIONS_PER_CHILD", "2");
    let _budget = RuntimeEnvGuard::set("RUNTIME_SINK_SESSION_BUDGET", "6");
    let temp = tempdir().unwrap();
    let marker_path = temp.path().join("multiplex-process-shrink.json");
    let sink = multiplex_test_sink(
        temp.path(),
        "multiplex_process_hold",
        &marker_path,
        "runtime_host_session_shrink",
    )
    .await;

    let (one, two, three, four, five, six) = tokio::join!(
        sink.sync(sample_stream(), "shrink-1".to_string(), None),
        sink.sync(sample_stream(), "shrink-2".to_string(), None),
        sink.sync(sample_stream(), "shrink-3".to_string(), None),
        sink.sync(sample_stream(), "shrink-4".to_string(), None),
        sink.sync(sample_stream(), "shrink-5".to_string(), None),
        sink.sync(sample_stream(), "shrink-6".to_string(), None),
    );
    one.unwrap();
    two.unwrap();
    three.unwrap();
    four.unwrap();
    five.unwrap();
    six.unwrap();
    assert_eq!(sink.worker_count_for_test(), 3);

    skipprd::metrics::counters::RUNTIME_SINK_POOL_TARGET.store(2, Ordering::SeqCst);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while sink.worker_count_for_test() != 1 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("idle maintenance should retire excess workers after backoff");
    assert_eq!(sink.worker_count_for_test(), 1);

    sink.sync(sample_stream(), "shrink-after-backoff".to_string(), None)
        .await
        .unwrap();
    assert_eq!(sink.worker_count_for_test(), 1);
}

#[tokio::test]
#[serial]
async fn one_multiplexed_session_failure_does_not_corrupt_another_ack() {
    let _target = RuntimeSinkPoolTargetGuard::set(2);
    let _pool_size = RuntimeEnvGuard::set("RUNTIME_SINK_CONNECTION_POOL_SIZE", "1");
    let _sessions = RuntimeEnvGuard::set("RUNTIME_SINK_SESSIONS_PER_CHILD", "2");
    let _budget = RuntimeEnvGuard::set("RUNTIME_SINK_SESSION_BUDGET", "2");
    let temp = tempdir().unwrap();
    let marker_path = temp.path().join("multiplex-one-failure.json");
    let sink = multiplex_test_sink(
        temp.path(),
        "multiplex_one_failure",
        &marker_path,
        "runtime_host_multiplex_one_failure",
    )
    .await;

    let delay = std::time::Duration::from_millis(20);
    let (failed, applied) = tokio::join!(
        sink.sync(
            delayed_sample_stream(delay),
            "multiplex-fail".to_string(),
            None
        ),
        sink.sync(
            delayed_sample_stream(delay),
            "multiplex-ok".to_string(),
            None
        ),
    );
    assert!(failed
        .unwrap_err()
        .to_string()
        .contains("simulated session failure"));
    applied.unwrap();
}

#[tokio::test]
#[serial]
async fn runtime_connection_death_fails_all_pending_sessions() {
    let _target = RuntimeSinkPoolTargetGuard::set(2);
    let _pool_size = RuntimeEnvGuard::set("RUNTIME_SINK_CONNECTION_POOL_SIZE", "1");
    let _sessions = RuntimeEnvGuard::set("RUNTIME_SINK_SESSIONS_PER_CHILD", "2");
    let _budget = RuntimeEnvGuard::set("RUNTIME_SINK_SESSION_BUDGET", "2");
    let _chunks = RuntimeEnvGuard::set("RUNTIME_SINK_PAYLOAD_CHUNK_BYTES", "64");
    let temp = tempdir().unwrap();
    let marker_path = temp.path().join("multiplex-death.json");
    let sink = multiplex_test_sink(
        temp.path(),
        "multiplex_disconnect_all",
        &marker_path,
        "runtime_host_multiplex_death",
    )
    .await;

    let delay = std::time::Duration::from_millis(20);
    let (first, second) = tokio::join!(
        sink.sync(delayed_sample_stream(delay), "death-a".to_string(), None),
        sink.sync(delayed_sample_stream(delay), "death-b".to_string(), None),
    );
    assert!(first.is_err());
    assert!(second.is_err());
}

#[tokio::test]
#[serial]
async fn schema_install_waits_for_active_multiplexed_applies() {
    let _target = RuntimeSinkPoolTargetGuard::set(2);
    let _pool_size = RuntimeEnvGuard::set("RUNTIME_SINK_CONNECTION_POOL_SIZE", "1");
    let _sessions = RuntimeEnvGuard::set("RUNTIME_SINK_SESSIONS_PER_CHILD", "2");
    let _budget = RuntimeEnvGuard::set("RUNTIME_SINK_SESSION_BUDGET", "2");
    let temp = tempdir().unwrap();
    let marker_path = temp.path().join("multiplex-schema-fence.json");
    let sink = multiplex_test_sink(
        temp.path(),
        "multiplex_schema_fence",
        &marker_path,
        "runtime_host_multiplex_schema_fence",
    )
    .await;
    let namespaces = BTreeMap::from([("people".to_string(), sample_output_metadata())]);

    let (apply, install) = tokio::join!(
        sink.sync(
            sample_stream(),
            "namespace=people&schema-fence".to_string(),
            None,
        ),
        async {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            sink.install_schema_state(1_000_000_001, &namespaces).await
        },
    );
    apply.unwrap();
    install.unwrap();

    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(marker_path).unwrap()).unwrap();
    let installs = json_u64_array(&state, "schema_install_active_counts");
    assert!(installs.len() >= 2);
    assert_eq!(*installs.last().unwrap(), 0);
}

#[tokio::test]
#[serial]
async fn unrelated_schema_install_proceeds_during_multiplexed_apply() {
    let _target = RuntimeSinkPoolTargetGuard::set(2);
    let _pool_size = RuntimeEnvGuard::set("RUNTIME_SINK_CONNECTION_POOL_SIZE", "1");
    let _sessions = RuntimeEnvGuard::set("RUNTIME_SINK_SESSIONS_PER_CHILD", "2");
    let _budget = RuntimeEnvGuard::set("RUNTIME_SINK_SESSION_BUDGET", "2");
    let temp = tempdir().unwrap();
    let marker_path = temp.path().join("multiplex-unrelated-schema.json");
    let sink = multiplex_test_sink(
        temp.path(),
        "multiplex_schema_fence",
        &marker_path,
        "runtime_host_multiplex_unrelated_schema",
    )
    .await;
    let namespaces = BTreeMap::from([("people".to_string(), sample_output_metadata())]);

    let (apply, install) = tokio::join!(
        sink.sync(
            sample_stream(),
            "namespace=events&unrelated-schema".to_string(),
            None,
        ),
        async {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            sink.install_schema_state(1_000_000_000, &namespaces).await
        },
    );
    apply.unwrap();
    install.unwrap();

    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(marker_path).unwrap()).unwrap();
    let installs = json_u64_array(&state, "schema_install_active_counts");
    assert!(installs.len() >= 2);
    assert!(
        *installs.last().unwrap() > 0,
        "unrelated namespace publication must not wait for the active apply"
    );
}

#[tokio::test]
#[serial]
async fn post_payload_disconnect_is_not_replayed_inside_same_call() {
    let _target = RuntimeSinkPoolTargetGuard::set(2);
    let _pool_size = RuntimeEnvGuard::set("RUNTIME_SINK_CONNECTION_POOL_SIZE", "1");
    let _sessions = RuntimeEnvGuard::set("RUNTIME_SINK_SESSIONS_PER_CHILD", "2");
    let _budget = RuntimeEnvGuard::set("RUNTIME_SINK_SESSION_BUDGET", "2");
    let temp = tempdir().unwrap();
    let marker_path = temp.path().join("multiplex-post-payload.json");
    let sink = multiplex_test_sink(
        temp.path(),
        "multiplex_disconnect_after_payload_once",
        &marker_path,
        "runtime_host_no_post_payload_retry",
    )
    .await;

    let err = sink
        .sync(sample_stream(), "post-payload-c=stable".to_string(), None)
        .await
        .unwrap_err();
    assert!(
        err.kind() == std::io::ErrorKind::UnexpectedEof
            || err.kind() == std::io::ErrorKind::BrokenPipe
            || err.to_string().contains("connection")
    );
}

#[tokio::test]
#[serial]
async fn receipt_mismatch_is_rejected_for_its_request() {
    let _pool_size = RuntimeEnvGuard::set("RUNTIME_SINK_CONNECTION_POOL_SIZE", "1");
    let temp = tempdir().unwrap();
    let marker_path = temp.path().join("multiplex-receipt-mismatch.json");
    let sink = multiplex_test_sink(
        temp.path(),
        "multiplex_receipt_mismatch",
        &marker_path,
        "runtime_host_receipt_mismatch",
    )
    .await;

    let err = sink
        .sync(sample_stream(), "receipt-mismatch".to_string(), None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("commit receipt does not match"));
}

#[tokio::test]
#[serial]
async fn unknown_response_request_id_is_a_protocol_error() {
    let _pool_size = RuntimeEnvGuard::set("RUNTIME_SINK_CONNECTION_POOL_SIZE", "1");
    let temp = tempdir().unwrap();
    let marker_path = temp.path().join("multiplex-request-mismatch.json");
    let sink = multiplex_test_sink(
        temp.path(),
        "multiplex_bad_request_id",
        &marker_path,
        "runtime_host_request_mismatch",
    )
    .await;

    let err = sink
        .sync(
            panic_on_poll_stream(),
            "request-id-mismatch".to_string(),
            None,
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("unknown request"));
}

#[tokio::test]
async fn prepare_rejection_leaves_payload_retryable() {
    let temp = tempdir().unwrap();
    let marker_path = temp.path().join("rejected-state.json");
    let manifest_path = write_sink_manifest(
        temp.path(),
        "rejected-runtime-sink",
        "File",
        "reject_prepare",
        "File",
        Some(&helper_sha256()),
        &helper_binary(),
        Some(&marker_path),
    );
    let sink = RuntimeDataSinkPlugin::new(
        ResolvedRuntimePlugin::load(&manifest_path).unwrap(),
        "runtime_host_rejected".to_string(),
        RuntimeBinding::Primary,
        runtime_file_sink_config(),
    )
    .await
    .unwrap();

    for attempt in 0..2 {
        let err = sink
            .sync(
                panic_on_poll_stream(),
                format!("rejected-c=retry-{attempt}"),
                None,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("prepare rejected"));
    }

    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(marker_path).unwrap()).unwrap();
    assert_eq!(state["run_count"], 0);
    assert_eq!(state["payload_bytes"], 0);
    assert_eq!(state["request_ids"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn stale_prepare_frame_restarts_before_payload_consumption() {
    let temp = tempdir().unwrap();
    let marker_path = temp.path().join("stale-prepare-once.marker");
    let manifest_path = write_sink_manifest(
        temp.path(),
        "stale-prepare-runtime-sink",
        "File",
        "stale_prepare_once",
        "File",
        Some(&helper_sha256()),
        &helper_binary(),
        Some(&marker_path),
    );
    let sink = RuntimeDataSinkPlugin::new(
        ResolvedRuntimePlugin::load(&manifest_path).unwrap(),
        "runtime_host_stale_prepare".to_string(),
        RuntimeBinding::Primary,
        runtime_file_sink_config(),
    )
    .await
    .unwrap();

    sink.sync(sample_stream(), "stale-prepare".to_string(), None)
        .await
        .unwrap();

    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(marker_path).unwrap()).unwrap();
    assert_eq!(state["run_count"], 1);
    assert!(state["payload_bytes"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn runtime_sink_global_budget_keeps_primary_and_deadletter_live() {
    let temp = tempdir().unwrap();
    let primary_marker = temp.path().join("primary-state.json");
    let deadletter_marker = temp.path().join("deadletter-state.json");
    let primary_manifest = write_sink_manifest(
        temp.path(),
        "budgeted-primary-runtime-sink",
        "File",
        "record_state",
        "File",
        Some(&helper_sha256()),
        &helper_binary(),
        Some(&primary_marker),
    );
    let deadletter_manifest = write_sink_manifest(
        temp.path(),
        "budgeted-deadletter-runtime-sink",
        "File",
        "record_state",
        "File",
        Some(&helper_sha256()),
        &helper_binary(),
        Some(&deadletter_marker),
    );
    let pipeline_name = "runtime_host_global_sink_budget".to_string();
    let primary = RuntimeDataSinkPlugin::new(
        ResolvedRuntimePlugin::load(&primary_manifest).unwrap(),
        pipeline_name.clone(),
        RuntimeBinding::Primary,
        runtime_file_sink_config(),
    )
    .await
    .unwrap();
    let deadletter = RuntimeDataSinkPlugin::new(
        ResolvedRuntimePlugin::load(&deadletter_manifest).unwrap(),
        pipeline_name,
        RuntimeBinding::Deadletter,
        runtime_file_sink_config(),
    )
    .await
    .unwrap();

    let (primary_result, deadletter_result) = tokio::join!(
        primary.sync(sample_stream(), "primary-budget-test".to_string(), None),
        deadletter.sync(sample_stream(), "deadletter-budget-test".to_string(), None),
    );
    primary_result.unwrap();
    deadletter_result.unwrap();

    let primary_state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(primary_marker).unwrap()).unwrap();
    let deadletter_state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(deadletter_marker).unwrap()).unwrap();
    assert_eq!(primary_state["install_request"]["binding"], "Primary");
    assert_eq!(deadletter_state["install_request"]["binding"], "Deadletter");
    assert_eq!(primary_state["run_count"], 1);
    assert_eq!(deadletter_state["run_count"], 1);
}

#[tokio::test]
async fn runtime_sink_restarts_after_io_disconnect() {
    let temp = tempdir().unwrap();
    let marker_path = temp.path().join("disconnect-once.marker");
    let manifest_path = write_sink_manifest(
        temp.path(),
        "disconnect-runtime-sink",
        "File",
        "disconnect_once",
        "File",
        Some(&helper_sha256()),
        &helper_binary(),
        Some(&marker_path),
    );

    let resolved = ResolvedRuntimePlugin::load(&manifest_path).unwrap();
    let sink = RuntimeDataSinkPlugin::new(
        resolved,
        "runtime_host_disconnect".to_string(),
        RuntimeBinding::Primary,
        runtime_file_sink_config(),
    )
    .await
    .unwrap();

    sink.sync(sample_stream(), "disconnect-test".to_string(), None)
        .await
        .unwrap();

    assert!(
        marker_path.exists(),
        "helper should have disconnected once before restart"
    );
}

#[tokio::test]
async fn runtime_sink_retries_schema_state_install_after_disconnect() {
    let temp = tempdir().unwrap();
    let marker_path = temp.path().join("schema-install-disconnect-once.marker");
    let manifest_path = write_sink_manifest(
        temp.path(),
        "schema-install-disconnect-runtime-sink",
        "File",
        "disconnect_on_schema_install_once",
        "File",
        Some(&helper_sha256()),
        &helper_binary(),
        Some(&marker_path),
    );

    let resolved = ResolvedRuntimePlugin::load(&manifest_path).unwrap();
    let sink = RuntimeDataSinkPlugin::new(
        resolved,
        "runtime_host_schema_install_disconnect".to_string(),
        RuntimeBinding::Primary,
        runtime_file_sink_config(),
    )
    .await
    .unwrap();

    let namespaces = BTreeMap::from([("people".to_string(), sample_output_metadata())]);
    sink.install_schema_state(7, &namespaces).await.unwrap();
    sink.sync(
        sample_stream(),
        "schema-install-disconnect-test".to_string(),
        None,
    )
    .await
    .unwrap();

    assert!(
        marker_path.exists(),
        "helper should have disconnected once during schema install"
    );
}

#[tokio::test]
async fn runtime_manifest_rejects_capability_drift() {
    let temp = tempdir().unwrap();
    let manifest_path = write_sink_manifest(
        temp.path(),
        "capability-drift-runtime-sink",
        "File",
        "normal",
        "Postgres",
        Some(&helper_sha256()),
        &helper_binary(),
        None,
    );

    let resolved = ResolvedRuntimePlugin::load(&manifest_path).unwrap();
    let err = match RuntimeDataSinkPlugin::new(
        resolved,
        "runtime_host_drift".to_string(),
        RuntimeBinding::Primary,
        runtime_file_sink_config(),
    )
    .await
    {
        Ok(_) => panic!("capability drift should fail during handshake"),
        Err(err) => err,
    };
    assert!(
        err.to_string().contains("sink capability mismatch"),
        "unexpected error: {}",
        err
    );
}

#[test]
fn runtime_manifest_rejects_v16_before_spawn() {
    let temp = tempdir().unwrap();
    let manifest_path = write_sink_manifest(
        temp.path(),
        "protocol-v16-runtime-sink",
        "File",
        "normal",
        "File",
        Some(&helper_sha256()),
        &helper_binary(),
        None,
    );
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    manifest["protocol_version"] = json!(16);
    std::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let err = ResolvedRuntimePlugin::load(&manifest_path).unwrap_err();
    assert!(err
        .to_string()
        .contains("uses protocol 16, but host requires protocol 17"));
}

#[tokio::test]
async fn runtime_manifest_rejects_bad_checksum() {
    let temp = tempdir().unwrap();
    let manifest_path = write_sink_manifest(
        temp.path(),
        "checksum-runtime-sink",
        "File",
        "normal",
        "File",
        Some("deadbeef"),
        &helper_binary(),
        None,
    );

    let resolved = ResolvedRuntimePlugin::load(&manifest_path).unwrap();
    let err = match RuntimeDataSinkPlugin::new(
        resolved,
        "runtime_host_checksum".to_string(),
        RuntimeBinding::Primary,
        runtime_file_sink_config(),
    )
    .await
    {
        Ok(_) => panic!("checksum mismatch should fail before spawn"),
        Err(err) => err,
    };
    assert!(
        err.to_string().contains("checksum mismatch"),
        "unexpected error: {}",
        err
    );
}

#[tokio::test]
async fn runtime_manifest_rejects_missing_artifact() {
    let temp = tempdir().unwrap();
    let manifest_path = write_sink_manifest(
        temp.path(),
        "missing-artifact-runtime-sink",
        "File",
        "normal",
        "File",
        None,
        &temp.path().join("missing-runtime-plugin"),
        None,
    );

    let resolved = ResolvedRuntimePlugin::load(&manifest_path).unwrap();
    let err = match RuntimeDataSinkPlugin::new(
        resolved,
        "runtime_host_missing_artifact".to_string(),
        RuntimeBinding::Primary,
        runtime_file_sink_config(),
    )
    .await
    {
        Ok(_) => panic!("missing artifact should fail before spawn"),
        Err(err) => err,
    };
    assert!(
        err.to_string().contains("No such file") || err.to_string().contains("not found"),
        "unexpected error: {}",
        err
    );
}

#[tokio::test]
async fn runtime_sink_installs_context_and_schema_state() {
    let temp = tempdir().unwrap();
    let marker_path = temp.path().join("install-state.json");
    let manifest_path = write_sink_manifest(
        temp.path(),
        "install-state-runtime-sink",
        "File",
        "record_state",
        "File",
        Some(&helper_sha256()),
        &helper_binary(),
        Some(&marker_path),
    );

    let resolved = ResolvedRuntimePlugin::load(&manifest_path).unwrap();
    let sink = RuntimeDataSinkPlugin::new(
        resolved,
        "runtime_host_context".to_string(),
        RuntimeBinding::Primary,
        runtime_file_sink_config(),
    )
    .await
    .unwrap();

    let namespaces = BTreeMap::from([("people".to_string(), sample_output_metadata())]);
    sink.install_schema_state(7, &namespaces).await.unwrap();
    sink.sync(sample_stream(), "context-test".to_string(), None)
        .await
        .unwrap();

    let state: serde_json::Value = serde_json::from_slice(&std::fs::read(&marker_path).unwrap())
        .expect("helper should write sink install state as JSON");
    assert_eq!(
        state["install_request"]["context"]["pipeline_name"],
        "runtime_host_context"
    );
    assert_eq!(state["install_request"]["binding"], "Primary");
    assert!(state["install_request"]["context"]["workspace_name"]
        .as_str()
        .is_some_and(|value| !value.is_empty()));
    assert!(state["install_request"]["context"]["data_dir"]
        .as_str()
        .is_some_and(|value| !value.is_empty()));
    assert_eq!(state["schema_state"]["version"], 7);
    assert!(state["schema_state"]["namespaces"]["people"].is_object());
}

#[tokio::test]
#[serial]
async fn runtime_sink_refreshes_schema_state_on_demand() {
    let temp = tempdir().unwrap();
    let marker_path = temp.path().join("refresh-state.json");
    let manifest_path = write_sink_manifest(
        temp.path(),
        "refresh-runtime-sink",
        "File",
        "refresh_once",
        "File",
        Some(&helper_sha256()),
        &helper_binary(),
        Some(&marker_path),
    );

    let resolved = ResolvedRuntimePlugin::load(&manifest_path).unwrap();
    let sink = RuntimeDataSinkPlugin::new(
        resolved,
        "runtime_host_refresh".to_string(),
        RuntimeBinding::Primary,
        runtime_file_sink_config(),
    )
    .await
    .unwrap();

    sink.sync(sample_stream(), "refresh-test".to_string(), None)
        .await
        .unwrap();
    sink.sync(sample_stream(), "refresh-test-2".to_string(), None)
        .await
        .unwrap();

    let state: serde_json::Value = serde_json::from_slice(&std::fs::read(&marker_path).unwrap())
        .expect("helper should write refresh state as JSON");
    assert_eq!(state["refresh_requested_once"], true);
    assert_eq!(state["run_count"], 2);
    assert!(state["schema_state"].is_object());
}

#[tokio::test]
#[serial]
async fn runtime_sink_reuses_compaction_id_across_replays() {
    let temp = tempdir().unwrap();
    let marker_path = temp.path().join("sink-compaction-ids.json");
    let manifest_path = write_sink_manifest(
        temp.path(),
        "sink-compaction-id-runtime-sink",
        "File",
        "record_state",
        "File",
        Some(&helper_sha256()),
        &helper_binary(),
        Some(&marker_path),
    );

    let resolved = ResolvedRuntimePlugin::load(&manifest_path).unwrap();
    let sink = RuntimeDataSinkPlugin::new(
        resolved,
        "runtime_host_compaction_id".to_string(),
        RuntimeBinding::Primary,
        runtime_file_sink_config(),
    )
    .await
    .unwrap();

    sink.sync(sample_stream(), "sink-replay-c=replay-42".to_string(), None)
        .await
        .unwrap();
    sink.sync(sample_stream(), "sink-replay-c=replay-42".to_string(), None)
        .await
        .unwrap();

    let state: serde_json::Value = serde_json::from_slice(&std::fs::read(&marker_path).unwrap())
        .expect("helper should write sink replay state as JSON");
    assert_eq!(state["run_count"], 2);

    let request_ids = json_u64_array(&state, "request_ids");
    assert_eq!(request_ids.len(), 2);
    assert_ne!(request_ids[0], request_ids[1]);

    let compaction_ids = json_string_array(&state, "compaction_ids");
    assert_eq!(
        compaction_ids,
        vec!["replay-42".to_string(), "replay-42".to_string()]
    );
}

#[tokio::test]
async fn runtime_schema_sink_restarts_after_io_disconnect() {
    let temp = tempdir().unwrap();
    let marker_path = temp.path().join("schema-disconnect-once.marker");
    let manifest_path = write_schema_manifest(
        temp.path(),
        "disconnect-runtime-schema-sink",
        "Glue",
        "disconnect_once",
        Some(&helper_sha256()),
        &helper_binary(),
        Some(&marker_path),
    );

    let resolved = ResolvedRuntimePlugin::load(&manifest_path).unwrap();
    let sink = RuntimeSchemaSinkPlugin::new(
        resolved,
        "runtime_schema_host_disconnect".to_string(),
        RuntimeBinding::Primary,
        runtime_glue_schema_config(),
    )
    .await
    .unwrap();

    sink.sync_schema("people", &sample_output_metadata())
        .await
        .unwrap();

    assert!(
        marker_path.exists(),
        "schema helper should have disconnected once before restart"
    );
}

#[tokio::test]
async fn runtime_schema_sink_reuses_compaction_id_across_replays() {
    let temp = tempdir().unwrap();
    let marker_path = temp.path().join("schema-compaction-ids.json");
    let manifest_path = write_schema_manifest(
        temp.path(),
        "schema-compaction-id-runtime-schema-sink",
        "Glue",
        "record_state",
        Some(&helper_sha256()),
        &helper_binary(),
        Some(&marker_path),
    );

    let resolved = ResolvedRuntimePlugin::load(&manifest_path).unwrap();
    let sink = RuntimeSchemaSinkPlugin::new(
        resolved,
        "runtime_schema_host_compaction_id".to_string(),
        RuntimeBinding::Primary,
        runtime_glue_schema_config(),
    )
    .await
    .unwrap();

    sink.sync_schema("people", &sample_output_metadata())
        .await
        .unwrap();
    sink.sync_schema("people", &sample_output_metadata())
        .await
        .unwrap();

    let state: serde_json::Value = serde_json::from_slice(&std::fs::read(&marker_path).unwrap())
        .expect("helper should write schema replay state as JSON");
    assert_eq!(state["run_count"], 2);

    let request_ids = json_u64_array(&state, "request_ids");
    assert_eq!(request_ids.len(), 2);
    assert_ne!(request_ids[0], request_ids[1]);

    let compaction_ids = json_string_array(&state, "compaction_ids");
    assert_eq!(compaction_ids.len(), 2);
    assert_eq!(compaction_ids[0], compaction_ids[1]);
    assert!(compaction_ids[0].starts_with("schema:primary:v"));
    assert!(compaction_ids[0].ends_with(":people"));
}

#[tokio::test]
async fn runtime_schema_sink_retries_schema_state_install_after_disconnect() {
    let temp = tempdir().unwrap();
    let marker_path = temp
        .path()
        .join("schema-state-install-disconnect-once.marker");
    let manifest_path = write_schema_manifest(
        temp.path(),
        "schema-install-disconnect-runtime-schema-sink",
        "Glue",
        "disconnect_on_schema_install_once",
        Some(&helper_sha256()),
        &helper_binary(),
        Some(&marker_path),
    );

    let resolved = ResolvedRuntimePlugin::load(&manifest_path).unwrap();
    let sink = RuntimeSchemaSinkPlugin::new(
        resolved,
        "runtime_schema_host_install_disconnect".to_string(),
        RuntimeBinding::Primary,
        runtime_glue_schema_config(),
    )
    .await
    .unwrap();

    let namespaces = BTreeMap::from([("people".to_string(), sample_output_metadata())]);
    sink.install_schema_state(7, &namespaces).await.unwrap();
    sink.sync_schema("people", &sample_output_metadata())
        .await
        .unwrap();

    assert!(
        marker_path.exists(),
        "schema helper should have disconnected once during schema install"
    );
}
