mod support;

use std::time::Duration;

use serial_test::serial;

use support::batch_e2e::{
    assert_success, fixture_path, output_buffer_dir, parquet_row_count_in_dir, BatchE2eHarness,
};

#[test]
#[serial]
fn file_csv_to_file_sink_writes_two_rows() {
    let pipeline_name = "batch_file_csv_to_file";
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

pipelines:
  {pipeline_name}:
    data_source: data_sources.file_source
    data_sink: data_sinks.file_sink
"#,
        input_path = fixture_path("people.csv").display(),
        pipeline_name = pipeline_name,
    );

    let harness = BatchE2eHarness::new(pipeline_name, &config);
    let output = harness.run_sync(Duration::from_secs(20));
    assert_success(&output);

    assert_eq!(parquet_row_count_in_dir(&output_buffer_dir(&harness)), 2);
}
