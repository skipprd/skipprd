use std::collections::BTreeMap;
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

struct RuntimeSinkPoolTargetGuard {
    previous: usize,
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
    skipprd::metrics::counters::RUNTIME_SINK_POOL_TARGET.store(2, Ordering::SeqCst);

    sink.sync(sample_stream(), "pool-growth-1".to_string(), None)
        .await
        .unwrap();
    assert_eq!(
        schema_install_count(&marker_path),
        startup_installs + 1,
        "only the new worker should receive the latest snapshot"
    );

    sink.sync(sample_stream(), "pool-growth-2".to_string(), None)
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
