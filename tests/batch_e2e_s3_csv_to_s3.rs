mod support;

use std::fs;
use std::time::Duration;

use support::batch_e2e::{
    assert_no_main_panic, ensure_s3_bucket, fixture_path, get_s3_object_bytes, list_s3_keys,
    localstack_s3_client, put_s3_object, BatchE2eHarness, LOCALSTACK_PORT,
};

#[ignore = "requires LocalStack docker service on port 14566"]
#[tokio::test]
async fn s3_csv_to_s3_emits_parquet_object() {
    let pipeline_name = "batch_s3_csv_to_s3";
    let suffix = rand::random::<u32>();
    let source_bucket = format!("skippr-batch-source-{}", suffix);
    let sink_bucket = format!("skippr-batch-sink-{}", suffix);
    let source_prefix = "incoming";
    let sink_prefix = "outgoing";
    let source_key = format!("{}/people.csv", source_prefix);
    let input = fs::read_to_string(fixture_path("people.csv")).unwrap();

    let client = localstack_s3_client(LOCALSTACK_PORT);
    ensure_s3_bucket(&client, &source_bucket).await;
    ensure_s3_bucket(&client, &sink_bucket).await;
    put_s3_object(&client, &source_bucket, &source_key, &input).await;

    let config = format!(
        r#"skippr:
  workspace: batch-tests
  storage_mode: local

data_sources:
  s3_source:
    S3:
      s3_bucket: {source_bucket}
      s3_prefix: "{source_prefix}/"
      endpoint_url: "http://127.0.0.1:{localstack_port}"
      format: csv

data_sinks:
  s3_sink:
    S3:
      s3_bucket: {sink_bucket}
      s3_prefix: "{sink_prefix}"
      endpoint_url: "http://127.0.0.1:{localstack_port}"

pipelines:
  {pipeline_name}:
    data_source: data_sources.s3_source
    data_sink: data_sinks.s3_sink
"#,
        source_bucket = source_bucket,
        source_prefix = source_prefix,
        sink_bucket = sink_bucket,
        sink_prefix = sink_prefix,
        localstack_port = LOCALSTACK_PORT,
        pipeline_name = pipeline_name,
    );

    let harness = BatchE2eHarness::new(pipeline_name, &config);
    let output = harness.run_sync_with_timeout(Duration::from_secs(8));
    assert_no_main_panic(&output);

    let keys = list_s3_keys(&client, &sink_bucket, sink_prefix).await;
    assert!(
        keys.iter()
            .any(|key| key.starts_with(&format!("{}/", sink_prefix)) && key.ends_with(".parquet")),
        "expected parquet object under prefix {}, got {:?}",
        sink_prefix,
        keys
    );

    let first_key = keys
        .into_iter()
        .find(|key| key.ends_with(".parquet"))
        .unwrap();
    let bytes = get_s3_object_bytes(&client, &sink_bucket, &first_key).await;
    assert!(!bytes.is_empty(), "expected parquet bytes in {}", first_key);
}
