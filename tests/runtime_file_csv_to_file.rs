mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;
use serial_test::serial;
use sha2::{Digest, Sha256};
use skippr::helpers::offsets::SLED_NAME;
use skippr::plugins::cdc::CheckpointEnvelope;
use skippr::runtime_plugins::manifest::RuntimePluginManifest;
use skippr::runtime_plugins::protocol::RUNTIME_PROTOCOL_VERSION;
use tempfile::tempdir;

use support::batch_e2e::{
    assert_no_main_panic, assert_success, fixture_path, output_buffer_dir,
    parquet_row_count_in_dir, BatchE2eHarness, TEST_WORKSPACE,
};

fn helper_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_skippr-runtime-plugin-test-helper"))
}

fn helper_sha256() -> String {
    let bytes = std::fs::read(helper_binary()).expect("failed to read runtime helper binary");
    format!("{:x}", Sha256::digest(bytes))
}

fn write_helper_manifest(
    dir: &Path,
    name: &str,
    kind: &str,
    cli_kind: &str,
    plugin_name: &str,
    scenario: &str,
) -> PathBuf {
    let manifest_path = dir.join(format!("{name}.json"));
    let mut artifacts = serde_json::Map::new();
    artifacts.insert(
        RuntimePluginManifest::current_target(),
        json!({
            "executable": helper_binary().display().to_string(),
            "sha256": helper_sha256(),
        }),
    );
    let manifest = json!({
        "name": name,
        "kind": kind,
        "plugin_name": plugin_name,
        "version": "test",
        "protocol_version": RUNTIME_PROTOCOL_VERSION,
        "config_schema_version": 1,
        "artifacts": artifacts,
        "args": [
            "--kind",
            cli_kind,
            "--plugin-name",
            plugin_name,
            "--scenario",
            scenario
        ],
        "supports_schema": false
    });
    std::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).expect("failed to serialize helper manifest"),
    )
    .expect("failed to write helper manifest");
    manifest_path
}

fn run_sync_with_logs(harness: &BatchE2eHarness, timeout: Duration) -> Output {
    let mut child = Command::new(PathBuf::from(env!("CARGO_BIN_EXE_skippr-el")))
        .args([
            "sync",
            "--pipeline",
            &harness.pipeline_name,
            "--output",
            "text",
            "--log",
        ])
        .env("SKIPPR_CONFIG_FILE", &harness.config_path)
        .env("DATA_DIR", &harness.data_dir)
        .env("AWS_ACCESS_KEY_ID", "test")
        .env("AWS_SECRET_ACCESS_KEY", "test")
        .env("AWS_DEFAULT_REGION", "us-east-1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn skippr-el");
    let deadline = Instant::now() + timeout;

    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().expect("failed to collect output"),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(200)),
            Ok(None) => {
                let _ = child.kill();
                let output = child.wait_with_output().expect("failed to collect output");
                panic!(
                    "skippr-el sync with logs timed out after {:?}\nstdout:\n{}\nstderr:\n{}",
                    timeout,
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            Err(err) => panic!("failed to poll child process: {}", err),
        }
    }
}

fn load_checkpoint_bytes(data_dir: &Path, pipeline_name: &str, key: &str) -> Option<Vec<u8>> {
    let db_path = data_dir
        .join(format!("{}_{}", TEST_WORKSPACE, pipeline_name))
        .join(SLED_NAME);
    let db = sled::open(db_path).ok()?;
    let tree = db.open_tree("offsets").ok()?;
    let checkpoint_key = format!("cdc_checkpoint:{key}");
    let value = tree.get(checkpoint_key.as_bytes()).ok().flatten()?;
    let envelope: CheckpointEnvelope = bincode::deserialize(&value).ok()?;
    envelope.into_payload::<Vec<u8>>().ok()
}

#[test]
#[serial]
fn file_csv_to_runtime_file_sink_writes_two_rows() {
    std::env::set_var("DATA_DIR_MIN_FREE_BYTES", "0");
    let pipeline_name = "runtime_file_csv_to_file";
    let manifest_dir = tempdir().expect("failed to create runtime manifest tempdir");
    let source_manifest_path = write_helper_manifest(
        manifest_dir.path(),
        "file-runtime-source",
        "DataSource",
        "data_source",
        "File",
        "emit_people_sink_write",
    );
    let sink_manifest_path = write_helper_manifest(
        manifest_dir.path(),
        "file-runtime-sink",
        "DataSink",
        "data_sink",
        "File",
        "write_output_buffer_parquet",
    );

    let config = format!(
        r#"skippr:
  workspace: batch-tests
  storage_mode: local

data_sources:
  file_source:
    File:
      path: "{input_path}"
      format: csv

data_sinks:
  file_sink:
    File:
      format: parquet

runtime_plugins:
  file_runtime_source:
    manifest: "{source_manifest_path}"
  file_runtime_sink:
    manifest: "{sink_manifest_path}"

pipelines:
  {pipeline_name}:
    data_source: data_sources.file_source
    data_sink: data_sinks.file_sink
    runtime_input: runtime_plugins.file_runtime_source
    runtime_output: runtime_plugins.file_runtime_sink
"#,
        input_path = fixture_path("people.csv").display(),
        source_manifest_path = source_manifest_path.display(),
        sink_manifest_path = sink_manifest_path.display(),
        pipeline_name = pipeline_name,
    );

    let harness = BatchE2eHarness::new(pipeline_name, &config);
    let output = run_sync_with_logs(&harness, Duration::from_secs(60));
    assert_success(&output);

    assert_eq!(parquet_row_count_in_dir(&output_buffer_dir(&harness)), 2);
    std::env::remove_var("DATA_DIR_MIN_FREE_BYTES");
}

#[test]
#[serial]
fn runtime_source_data_channel_checkpoint_is_persisted() {
    std::env::set_var("DATA_DIR_MIN_FREE_BYTES", "0");
    let pipeline_name = "runtime_source_data_channel_checkpoint";
    let manifest_dir = tempdir().expect("failed to create runtime manifest tempdir");
    let source_manifest_path = write_helper_manifest(
        manifest_dir.path(),
        "file-runtime-source-checkpoint",
        "DataSource",
        "data_source",
        "File",
        "emit_people_sink_write_and_checkpoint",
    );
    let sink_manifest_path = write_helper_manifest(
        manifest_dir.path(),
        "file-runtime-sink-checkpoint",
        "DataSink",
        "data_sink",
        "File",
        "write_output_buffer_parquet",
    );

    let config = format!(
        r#"skippr:
  workspace: batch-tests
  storage_mode: local

data_sources:
  file_source:
    File:
      path: "{input_path}"
      format: csv

data_sinks:
  file_sink:
    File:
      format: parquet

runtime_plugins:
  file_runtime_source:
    manifest: "{source_manifest_path}"
  file_runtime_sink:
    manifest: "{sink_manifest_path}"

pipelines:
  {pipeline_name}:
    data_source: data_sources.file_source
    data_sink: data_sinks.file_sink
    runtime_input: runtime_plugins.file_runtime_source
    runtime_output: runtime_plugins.file_runtime_sink
"#,
        input_path = fixture_path("people.csv").display(),
        source_manifest_path = source_manifest_path.display(),
        sink_manifest_path = sink_manifest_path.display(),
        pipeline_name = pipeline_name,
    );

    let harness = BatchE2eHarness::new(pipeline_name, &config);
    let output = run_sync_with_logs(&harness, Duration::from_secs(60));
    assert_success(&output);

    assert_eq!(parquet_row_count_in_dir(&output_buffer_dir(&harness)), 2);
    assert_eq!(
        load_checkpoint_bytes(
            harness.data_dir(),
            pipeline_name,
            "runtime-helper-source-checkpoint"
        ),
        Some(b"checkpoint".to_vec())
    );
    std::env::remove_var("DATA_DIR_MIN_FREE_BYTES");
}

#[test]
#[serial]
fn runtime_source_handshake_failure_exits_nonzero() {
    std::env::set_var("DATA_DIR_MIN_FREE_BYTES", "0");
    let pipeline_name = "runtime_source_handshake_failure";
    let manifest_dir = tempdir().expect("failed to create runtime manifest tempdir");
    let source_manifest_path = write_helper_manifest(
        manifest_dir.path(),
        "file-runtime-source-bad-handshake",
        "DataSource",
        "data_source",
        "File",
        "handshake_error_frame",
    );
    let sink_manifest_path = write_helper_manifest(
        manifest_dir.path(),
        "file-runtime-sink-normal",
        "DataSink",
        "data_sink",
        "File",
        "normal",
    );

    let config = format!(
        r#"skippr:
  workspace: batch-tests
  storage_mode: local

data_sources:
  file_source:
    File:
      path: "{input_path}"
      format: csv

data_sinks:
  file_sink:
    File:
      format: parquet

runtime_plugins:
  file_runtime_source:
    manifest: "{source_manifest_path}"
  file_runtime_sink:
    manifest: "{sink_manifest_path}"

pipelines:
  {pipeline_name}:
    data_source: data_sources.file_source
    data_sink: data_sinks.file_sink
    runtime_input: runtime_plugins.file_runtime_source
    runtime_output: runtime_plugins.file_runtime_sink
"#,
        input_path = fixture_path("people.csv").display(),
        source_manifest_path = source_manifest_path.display(),
        sink_manifest_path = sink_manifest_path.display(),
        pipeline_name = pipeline_name,
    );

    let harness = BatchE2eHarness::new(pipeline_name, &config);
    let output = harness.run_sync(Duration::from_secs(60));
    assert!(
        !output.status.success(),
        "skippr-el sync unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "runtime source handshake failure should exit with status 1\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_no_main_panic(&output);

    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !combined.contains("Pipeline sync complete"),
        "sync completion should not be logged after a runtime source handshake failure\n{}",
        combined
    );
    std::env::remove_var("DATA_DIR_MIN_FREE_BYTES");
}
