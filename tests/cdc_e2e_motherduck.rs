/// CDC End-to-End: Postgres source → Motherduck sink
///
/// Requires: `postgres` (port 15432) and valid Motherduck token.
/// Env-gated: skips when MOTHERDUCK_TOKEN absent.
///
///   cargo test --test cdc_e2e_motherduck -- --ignored
mod support;

use std::time::Duration;
use support::cdc_e2e::*;

const PG_SOURCE_PORT: u16 = 15432;

fn unique_table(base: &str) -> String {
    format!("{}_{}", base, rand::random::<u32>())
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
async fn e2e_postgres_to_motherduck_cdc_snapshot() {
    let token = match std::env::var("MOTHERDUCK_TOKEN") {
        Ok(v) => v,
        Err(_) => {
            eprintln!("MOTHERDUCK_TOKEN not set — skipping");
            return;
        }
    };

    let table = unique_table("cdc_md");

    cleanup_replication("skippr_slot", "skippr_publication").await;
    setup_pg_source(&table).await;
    seed_pg_rows(&table, &[(1, "Alice", "London"), (2, "Bob", "Paris")]).await;

    let config = pg_to_motherduck_cdc_config_yaml(
        PG_SOURCE_PORT,
        "cdc_pg_md",
        &[&table],
        &["id"],
        &token,
        "skippr_e2e",
    );

    let harness = CdcE2eHarness::new("cdc_pg_md", &config);
    let stderr = harness.run_sync_with_timeout(Duration::from_secs(30));
    eprintln!("--- skippr-el stderr ---\n{}", stderr);

    cleanup_replication("skippr_slot", "skippr_publication").await;

    assert!(
        !stderr.contains("CDC validation failed"),
        "CDC validation should pass for Postgres->Motherduck"
    );
    assert!(!stderr.contains("panic"), "pipeline should not panic");
}
