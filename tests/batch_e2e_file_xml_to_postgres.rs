mod support;

use std::time::Duration;

use support::batch_e2e::{assert_no_main_panic, fixture_path, BatchE2eHarness, POSTGRES_PORT};
use support::cdc_e2e::{pg_client, pg_execute, pg_row_exists, pg_wait_for_rows};

#[ignore = "requires postgres docker service on port 15432"]
#[tokio::test]
async fn file_xml_to_postgres_inserts_two_rows() {
    let pipeline_name = "batch_file_xml_to_postgres";
    let client = pg_client(POSTGRES_PORT).await;
    pg_execute(&client, &format!("DROP TABLE IF EXISTS {}", pipeline_name)).await;

    let config = format!(
        r#"skippr:
  workspace: batch-tests
  storage_mode: local

data_sources:
  file_source:
    File:
      path: "{input_path}"
      format: xml

data_sinks:
  pg_sink:
    Postgres:
      host: 127.0.0.1
      port: {pg_port}
      user: postgres
      password: testpass
      database: skippr_test

pipelines:
  {pipeline_name}:
    data_source: data_sources.file_source
    data_sink: data_sinks.pg_sink
"#,
        input_path = fixture_path("catalog.xml").display(),
        pg_port = POSTGRES_PORT,
        pipeline_name = pipeline_name,
    );

    let harness = BatchE2eHarness::new(pipeline_name, &config);
    let output = harness.run_sync_with_timeout(Duration::from_secs(8));
    assert_no_main_panic(&output);

    assert_eq!(
        pg_wait_for_rows(&client, pipeline_name, 2, Duration::from_secs(20)).await,
        2
    );
    assert!(pg_row_exists(&client, pipeline_name, "name = 'Ada Lovelace'").await);
    assert!(pg_row_exists(&client, pipeline_name, "name = 'Grace Hopper'").await);
}
