mod support;

use std::time::Duration;

use serial_test::serial;

use support::batch_e2e::{
    assert_success, fixture_path, output_buffer_dir, parquet_row_count_in_dir, BatchE2eHarness,
};
use support::runtime_plugins::manifest_path_from_env;

#[test]
#[serial]
fn file_csv_to_runtime_file_sink_writes_two_rows() {
    std::env::set_var("DATA_DIR_MIN_FREE_BYTES", "0");
    let pipeline_name = "runtime_file_csv_to_file";
    let source_manifest_path = manifest_path_from_env(
        "SKIPPR_RUNTIME_FILE_SOURCE_MANIFEST",
        "runtime_plugins/manifests/file-source.json",
    );
    let manifest_path = manifest_path_from_env(
        "SKIPPR_RUNTIME_FILE_SINK_MANIFEST",
        "runtime_plugins/manifests/file-sink.json",
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
    manifest: "{manifest_path}"

pipelines:
  {pipeline_name}:
    data_source: data_sources.file_source
    data_sink: data_sinks.file_sink
    runtime_input: runtime_plugins.file_runtime_source
    runtime_output: runtime_plugins.file_runtime_sink
"#,
        input_path = fixture_path("people.csv").display(),
        source_manifest_path = source_manifest_path.display(),
        manifest_path = manifest_path.display(),
        pipeline_name = pipeline_name,
    );

    let harness = BatchE2eHarness::new(pipeline_name, &config);
    let output = harness.run_sync(Duration::from_secs(20));
    assert_success(&output);

    assert_eq!(parquet_row_count_in_dir(&output_buffer_dir(&harness)), 2);
    std::env::remove_var("DATA_DIR_MIN_FREE_BYTES");
}
