/// CDC End-to-End: Postgres source → Snowflake sink
///
/// Requires: `postgres` (port 15432) and valid Snowflake credentials via env vars.
///
///   cargo test --test cdc_e2e_snowflake -- --ignored
mod support;

use std::time::Duration;
use support::cdc_e2e::*;

const PG_SOURCE_PORT: u16 = 15432;

fn unique_table(base: &str) -> String {
    format!("{}_{}", base, rand::random::<u32>())
}

fn snowflake_config_yaml(
    pg_source_port: u16,
    pipeline_name: &str,
    source_table: &str,
    business_keys: &[&str],
) -> String {
    let bk_yaml: String = business_keys
        .iter()
        .map(|k| format!("\"{}\"", k))
        .collect::<Vec<_>>()
        .join(", ");

    format!(
        r#"data_sources:
  pg_cdc_source:
    Postgres:
      host: 127.0.0.1
      port: {pg_source_port}
      user: postgres
      password: testpass
      database: skippr_test
      tables: ["{source_table}"]

data_sinks:
  sf_cdc_target:
    Snowflake:
      account: "${{SNOWFLAKE_ACCOUNT}}"
      user: "${{SNOWFLAKE_USER}}"
      private_key_path: "${{SNOWFLAKE_PRIVATE_KEY_PATH}}"
      database: SKIPPR_E2E
      schema: PUBLIC
      warehouse: COMPUTE_WH
      role: ACCOUNTADMIN

pipelines:
  {pipeline_name}:
    data_source: data_sources.pg_cdc_source
    data_sink: data_sinks.sf_cdc_target
    cdc:
      business_key_columns: [{bk_yaml}]
"#
    )
}

async fn setup_pg_source(table: &str) {
    let client = pg_client(PG_SOURCE_PORT).await;
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

async fn seed_pg_rows(table: &str, rows: &[(i64, &str, &str)]) {
    let client = pg_client(PG_SOURCE_PORT).await;
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

async fn cleanup_replication(slot: &str, pub_name: &str) {
    let client = pg_client(PG_SOURCE_PORT).await;
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

#[tokio::test]
#[ignore]
async fn e2e_postgres_to_snowflake_cdc_snapshot() {
    if std::env::var("SNOWFLAKE_ACCOUNT").is_err() {
        eprintln!("SNOWFLAKE_ACCOUNT not set — skipping");
        return;
    }

    let table = unique_table("cdc_sf");

    cleanup_replication("skippr_slot", "skippr_publication").await;
    setup_pg_source(&table).await;
    seed_pg_rows(&table, &[(1, "Alice", "London"), (2, "Bob", "Paris")]).await;

    let config = snowflake_config_yaml(PG_SOURCE_PORT, "cdc_pg_sf", &table, &["id"]);

    let harness = CdcE2eHarness::new("cdc_pg_sf", &config);
    let stderr = harness.run_sync_with_timeout(Duration::from_secs(30));
    eprintln!("--- skippr-el stderr ---\n{}", stderr);

    cleanup_replication("skippr_slot", "skippr_publication").await;

    // Snowflake assertions would require a Snowflake client; for now the test
    // validates that the pipeline runs without errors.
    assert!(
        !stderr.contains("CDC validation failed"),
        "CDC validation should pass for Postgres->Snowflake"
    );
    assert!(!stderr.contains("panic"), "pipeline should not panic");
}
