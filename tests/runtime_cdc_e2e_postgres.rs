mod support;

use std::path::PathBuf;
use std::time::Duration;

use serial_test::serial;
use support::batch_e2e::{parquet_column_names_in_dir, parquet_row_count_in_dir, TEST_WORKSPACE};
use support::cdc_e2e::{
    pg_client, pg_column_exists, pg_execute, pg_row_count, pg_table_exists, CdcE2eHarness,
};
use support::runtime_plugins::manifest_path_from_env;

const SOURCE_PORT: u16 = 15432;
const TARGET_PORT: u16 = 15433;

fn unique_table(base: &str) -> String {
    format!("{}_{}", base, rand::random::<u32>())
}

async fn setup_source_table(table: &str) {
    let client = pg_client(SOURCE_PORT).await;
    pg_execute(
        &client,
        &format!(
            "DROP TABLE IF EXISTS {} CASCADE;
             CREATE TABLE {} (id BIGINT PRIMARY KEY, name TEXT, city TEXT);",
            table, table
        ),
    )
    .await;
}

async fn seed_source_rows(table: &str, rows: &[(i64, &str, &str)]) {
    let client = pg_client(SOURCE_PORT).await;
    for (id, name, city) in rows {
        pg_execute(
            &client,
            &format!(
                "INSERT INTO {} (id, name, city) VALUES ({}, '{}', '{}')",
                table, id, name, city
            ),
        )
        .await;
    }
}

async fn cleanup_target(table: &str) {
    let client = pg_client(TARGET_PORT).await;
    let _ = client
        .batch_execute(&format!(
            "DROP TABLE IF EXISTS \"public\".\"postgres.{}\" CASCADE",
            table
        ))
        .await;
    let tombstone = format!("_skippr_tombstones_postgres.{}", table);
    let _ = client
        .batch_execute(&format!(
            "DROP TABLE IF EXISTS \"public\".\"{}\" CASCADE",
            tombstone
        ))
        .await;
}

async fn cleanup_source_replication(slot: &str, pub_name: &str) {
    let client = pg_client(SOURCE_PORT).await;
    let _ = client
        .batch_execute(&format!(
            "SELECT pg_drop_replication_slot('{}') WHERE EXISTS (SELECT 1 FROM pg_replication_slots WHERE slot_name = '{}')",
            slot, slot
        ))
        .await;
    let _ = client
        .batch_execute(&format!("DROP PUBLICATION IF EXISTS {}", pub_name))
        .await;
}

async fn runtime_pg_services_available() -> bool {
    async fn can_connect(port: u16) -> bool {
        tokio_postgres::connect(
            &format!(
                "host=127.0.0.1 port={} user=postgres password=testpass dbname=skippr_test",
                port
            ),
            tokio_postgres::NoTls,
        )
        .await
        .is_ok()
    }

    can_connect(SOURCE_PORT).await && can_connect(TARGET_PORT).await
}

fn runtime_pg_cdc_config_yaml(table: &str, pipeline_name: &str) -> String {
    let source_manifest = manifest_path_from_env(
        "SKIPPR_RUNTIME_POSTGRES_SOURCE_MANIFEST",
        "runtime_plugins/manifests/postgres-source.json",
    );
    let sink_manifest = manifest_path_from_env(
        "SKIPPR_RUNTIME_POSTGRES_SINK_MANIFEST",
        "runtime_plugins/manifests/postgres-sink.json",
    );
    let schema_manifest = manifest_path_from_env(
        "SKIPPR_RUNTIME_POSTGRES_SCHEMA_MANIFEST",
        "runtime_plugins/manifests/postgres-schema.json",
    );

    format!(
        r#"skippr:
  workspace: batch-tests
  storage_mode: local

data_sources:
  pg_cdc_source:
    Postgres:
      host: 127.0.0.1
      port: {source_port}
      user: postgres
      password: testpass
      database: skippr_test
      tables: ["{table}"]
      cdc_enabled: true

data_sinks:
  pg_cdc_target:
    Postgres:
      host: 127.0.0.1
      port: {target_port}
      user: postgres
      password: testpass
      database: skippr_test
      schema: public

runtime_plugins:
  pg_runtime_source:
    manifest: "{source_manifest}"
  pg_runtime_sink:
    manifest: "{sink_manifest}"
  pg_runtime_schema:
    manifest: "{schema_manifest}"

pipelines:
  {pipeline_name}:
    data_source: data_sources.pg_cdc_source
    data_sink: data_sinks.pg_cdc_target
    runtime_input: runtime_plugins.pg_runtime_source
    runtime_output: runtime_plugins.pg_runtime_sink
    runtime_schema_sink: runtime_plugins.pg_runtime_schema
    cdc:
      business_key_columns: ["id"]
"#,
        source_port = SOURCE_PORT,
        target_port = TARGET_PORT,
        table = table,
        source_manifest = source_manifest.display(),
        sink_manifest = sink_manifest.display(),
        schema_manifest = schema_manifest.display(),
        pipeline_name = pipeline_name,
    )
}

fn runtime_pg_to_file_cdc_config_yaml(table: &str, pipeline_name: &str) -> String {
    let source_manifest = manifest_path_from_env(
        "SKIPPR_RUNTIME_POSTGRES_SOURCE_MANIFEST",
        "runtime_plugins/manifests/postgres-source.json",
    );
    let sink_manifest = manifest_path_from_env(
        "SKIPPR_RUNTIME_FILE_SINK_MANIFEST",
        "runtime_plugins/manifests/file-sink.json",
    );

    format!(
        r#"skippr:
  workspace: batch-tests
  storage_mode: local

data_sources:
  pg_cdc_source:
    Postgres:
      host: 127.0.0.1
      port: {source_port}
      user: postgres
      password: testpass
      database: skippr_test
      tables: ["{table}"]
      cdc_enabled: true

data_sinks:
  file_target:
    File:
      format: parquet

runtime_plugins:
  pg_runtime_source:
    manifest: "{source_manifest}"
  file_runtime_sink:
    manifest: "{sink_manifest}"

pipelines:
  {pipeline_name}:
    data_source: data_sources.pg_cdc_source
    data_sink: data_sinks.file_target
    runtime_input: runtime_plugins.pg_runtime_source
    runtime_output: runtime_plugins.file_runtime_sink
    cdc:
      business_key_columns: ["id"]
"#,
        source_port = SOURCE_PORT,
        table = table,
        source_manifest = source_manifest.display(),
        sink_manifest = sink_manifest.display(),
        pipeline_name = pipeline_name,
    )
}

fn runtime_output_buffer_dir(harness: &CdcE2eHarness) -> PathBuf {
    harness
        .data_dir()
        .join(format!("{}_{}", TEST_WORKSPACE, harness.pipeline_name))
        .join("output_buffer")
}

#[tokio::test]
#[serial]
async fn runtime_postgres_cdc_snapshot_populates_order_token() {
    if !runtime_pg_services_available().await {
        eprintln!("Skipping runtime Postgres CDC test because source/target Postgres services are unavailable");
        return;
    }
    std::env::set_var("DATA_DIR_MIN_FREE_BYTES", "0");
    let table = unique_table("runtime_cdc_snap");
    cleanup_source_replication("skippr_slot", "skippr_publication").await;
    setup_source_table(&table).await;
    seed_source_rows(
        &table,
        &[
            (1, "Alice", "London"),
            (2, "Bob", "Paris"),
            (3, "Charlie", "Berlin"),
        ],
    )
    .await;
    cleanup_target(&table).await;

    let pipeline_name = "runtime_cdc_pg_snap";
    let config = runtime_pg_cdc_config_yaml(&table, pipeline_name);
    let harness = CdcE2eHarness::new(pipeline_name, &config);

    let stderr = harness.run_sync_with_timeout(Duration::from_secs(20));
    assert!(
        !stderr.contains("CDC validation failed"),
        "runtime CDC validation should pass:\n{}",
        stderr
    );

    let target_table_name = format!("postgres.{}", table);
    let fq = format!("\"public\".\"{}\"", target_table_name);
    let target = pg_client(TARGET_PORT).await;
    let count = pg_row_count(&target, &fq).await;
    assert!(
        count >= 3,
        "expected at least 3 rows in target, got {}",
        count
    );

    let has_order_token =
        pg_column_exists(&target, &target_table_name, "_skippr_order_token").await;
    assert!(
        has_order_token,
        "runtime sink should add _skippr_order_token"
    );

    let tombstone_table = format!("_skippr_tombstones_{}", target_table_name);
    let has_tombstone = pg_table_exists(&target, &tombstone_table).await;
    assert!(has_tombstone, "runtime sink should ensure tombstone table");

    cleanup_source_replication("skippr_slot", "skippr_publication").await;
    std::env::remove_var("DATA_DIR_MIN_FREE_BYTES");
}

#[tokio::test]
#[serial]
async fn runtime_postgres_cdc_to_file_sink_writes_cdc_columns() {
    if !runtime_pg_services_available().await {
        eprintln!("Skipping runtime Postgres-to-File CDC test because source/target Postgres services are unavailable");
        return;
    }

    std::env::set_var("DATA_DIR_MIN_FREE_BYTES", "0");
    let table = unique_table("runtime_cdc_file");
    cleanup_source_replication("skippr_slot", "skippr_publication").await;
    setup_source_table(&table).await;
    seed_source_rows(
        &table,
        &[
            (1, "Alice", "London"),
            (2, "Bob", "Paris"),
            (3, "Charlie", "Berlin"),
        ],
    )
    .await;

    let pipeline_name = "runtime_cdc_pg_file";
    let config = runtime_pg_to_file_cdc_config_yaml(&table, pipeline_name);
    let harness = CdcE2eHarness::new(pipeline_name, &config);

    let stderr = harness.run_sync_with_timeout(Duration::from_secs(20));
    assert!(
        !stderr.contains("CDC validation failed"),
        "runtime CDC validation should pass:\n{}",
        stderr
    );

    let output_dir = runtime_output_buffer_dir(&harness);
    assert!(
        parquet_row_count_in_dir(&output_dir) >= 3,
        "expected at least 3 runtime CDC rows in file output"
    );
    let columns = parquet_column_names_in_dir(&output_dir);
    assert!(
        columns.iter().any(|column| column == "_skippr_mutation"),
        "runtime file sink should write _skippr_mutation for CDC payloads: {:?}",
        columns
    );
    assert!(
        columns.iter().any(|column| column == "_skippr_order_token"),
        "runtime file sink should write _skippr_order_token for CDC payloads: {:?}",
        columns
    );

    cleanup_source_replication("skippr_slot", "skippr_publication").await;
    std::env::remove_var("DATA_DIR_MIN_FREE_BYTES");
}
