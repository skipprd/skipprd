use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use arrow::array::{Int64Array, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use datafusion::error::DataFusionError;
use datafusion::physical_plan::{RecordBatchStream, SendableRecordBatchStream};
use futures::Stream;
use serde_json::json;
use sha2::{Digest, Sha256};
use skippr::plugins::DataSink;
use skippr::runtime_plugins::host::{ResolvedRuntimePlugin, RuntimeDataSinkPlugin};
use skippr::runtime_plugins::manifest::RuntimePluginManifest;
use skippr::runtime_plugins::protocol::{
    RuntimeBinding, RuntimePluginConfigEnvelope, RuntimeSinkConfig,
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
            "supports_transactions": true
        }),
        _ => json!({
            "name": "File",
            "guarantee_tier": "CdcEncodedOnly",
            "can_manage_skippr_columns": false,
            "can_maintain_tombstone_tables": false,
            "can_compare_order_tokens": false,
            "supports_transactions": false
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
        "protocol_version": 1,
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

fn runtime_file_sink_config() -> RuntimeSinkConfig {
    RuntimeSinkConfig(RuntimePluginConfigEnvelope::new(
        "File",
        json!({
            "format": "parquet",
            "output_dir": null
        }),
    ))
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
