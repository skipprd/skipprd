/// CDC End-to-End: Postgres source → BigQuery sink
///
/// Requires: `postgres` (port 15432) and valid BigQuery credentials
/// (GOOGLE_APPLICATION_CREDENTIALS env var pointing at a service account key).
///
///   cargo test --test cdc_e2e_bigquery -- --ignored
mod support;

use std::time::Duration;
use support::cdc_e2e::*;

const PG_SOURCE_PORT: u16 = 15432;

fn unique_table(base: &str) -> String {
    format!("{}_{}", base, rand::random::<u32>())
}

fn bigquery_config_yaml(
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
  bq_cdc_target:
    Bigquery:
      project: optimistic-jet-274810
      dataset: newyork
      location: US

pipelines:
  {pipeline_name}:
    data_source: data_sources.pg_cdc_source
    data_sink: data_sinks.bq_cdc_target
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
async fn e2e_postgres_to_bigquery_cdc_snapshot() {
    if std::env::var("GOOGLE_APPLICATION_CREDENTIALS").is_err() {
        eprintln!("GOOGLE_APPLICATION_CREDENTIALS not set — skipping");
        return;
    }

    let table = unique_table("cdc_bq");

    cleanup_replication("skippr_slot", "skippr_publication").await;
    setup_pg_source(&table).await;
    seed_pg_rows(&table, &[(1, "Alice", "London"), (2, "Bob", "Paris")]).await;

    let config = bigquery_config_yaml(PG_SOURCE_PORT, "cdc_pg_bq", &table, &["id"]);

    let harness = CdcE2eHarness::new("cdc_pg_bq", &config);
    let stderr = harness.run_sync_with_timeout(Duration::from_secs(30));
    eprintln!("--- skippr-el stderr ---\n{}", stderr);

    cleanup_replication("skippr_slot", "skippr_publication").await;

    assert!(
        !stderr.contains("CDC validation failed"),
        "CDC validation should pass for Postgres->BigQuery"
    );
    assert!(!stderr.contains("panic"), "pipeline should not panic");
}
