/// CDC End-to-End: Postgres source → ClickHouse sink
///
/// Requires Docker services: `postgres` (port 15432) and `clickhouse` (port 18123).
///
///   cargo test --test cdc_e2e_clickhouse -- --ignored
mod support;

use std::time::Duration;
use support::cdc_e2e::*;

const PG_SOURCE_PORT: u16 = 15432;
const CH_PORT: u16 = 18123;

fn unique_table(base: &str) -> String {
    format!("{}_{}", base, rand::random::<u32>())
}

async fn ch_execute(sql: &str) {
    let output = std::process::Command::new("docker")
        .args([
            "exec",
            "skipprd_clickhouse_1",
            "clickhouse-client",
            "--query",
            sql,
        ])
        .output()
        .expect("docker exec failed");
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.contains("doesn't exist") {
            panic!("ClickHouse query failed: {}\nSQL: {}", stderr, sql);
        }
    }
}

async fn ch_query_count(table: &str) -> i64 {
    let output = std::process::Command::new("docker")
        .args([
            "exec",
            "skipprd_clickhouse_1",
            "clickhouse-client",
            "--query",
            &format!("SELECT count() FROM {}", table),
        ])
        .output()
        .expect("docker exec failed");
    if !output.status.success() {
        return 0;
    }
    let out = String::from_utf8_lossy(&output.stdout);
    out.trim().parse().unwrap_or(0)
}

async fn ch_column_exists(table: &str, column: &str) -> bool {
    let output = std::process::Command::new("docker")
        .args([
            "exec",
            "skipprd_clickhouse_1",
            "clickhouse-client",
            "--query",
            &format!(
                "SELECT 1 FROM system.columns WHERE table='{}' AND name='{}'",
                table, column
            ),
        ])
        .output()
        .expect("docker exec failed");
    let out = String::from_utf8_lossy(&output.stdout);
    !out.trim().is_empty()
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

async fn cleanup_source_replication(slot: &str, pub_name: &str) {
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
async fn e2e_postgres_to_clickhouse_cdc_snapshot() {
    let table = unique_table("cdc_ch");

    cleanup_source_replication("skippr_slot", "skippr_publication").await;
    setup_pg_source(&table).await;
    seed_pg_rows(&table, &[(1, "Alice", "London"), (2, "Bob", "Paris")]).await;

    let ch_table = format!("postgres.{}", table);
    ch_execute(&format!("DROP TABLE IF EXISTS default.`{}`", ch_table)).await;

    let config =
        pg_to_clickhouse_cdc_config_yaml(PG_SOURCE_PORT, CH_PORT, "cdc_pg_ch", &[&table], &["id"]);

    let harness = CdcE2eHarness::new("cdc_pg_ch", &config);
    let stderr = harness.run_sync_with_timeout(Duration::from_secs(25));
    eprintln!("--- skippr-el stderr ---\n{}", stderr);

    let count = ch_query_count(&format!("default.`{}`", ch_table)).await;
    assert!(
        count >= 2,
        "expected at least 2 rows in ClickHouse, got {}",
        count
    );

    let has_token = ch_column_exists(&ch_table, "_skippr_order_token").await;
    assert!(
        has_token,
        "ClickHouse table should have _skippr_order_token column"
    );

    cleanup_source_replication("skippr_slot", "skippr_publication").await;
}

#[tokio::test]
#[ignore]
async fn e2e_postgres_to_clickhouse_cdc_insert_update_delete_lifecycle() {
    let table = unique_table("ch_life");

    cleanup_source_replication("skippr_slot", "skippr_publication").await;
    setup_pg_source(&table).await;
    seed_pg_rows(
        &table,
        &[
            (1, "Alice", "London"),
            (2, "Bob", "Paris"),
            (3, "Carol", "Berlin"),
        ],
    )
    .await;

    ch_exec(
        CH_PORT,
        &format!("DROP TABLE IF EXISTS \"default\".\"postgres.{}\"", table),
    )
    .await;

    let config =
        pg_to_clickhouse_cdc_config_yaml(PG_SOURCE_PORT, CH_PORT, "ch_life", &[&table], &["id"]);

    let ch_fq = format!("\"default\".\"postgres.{}\"", table);

    let harness = CdcE2eHarness::new("ch_life", &config);
    let stderr = harness.run_sync_with_timeout(Duration::from_secs(25));
    eprintln!("--- skippr-el stderr (lifecycle sync 1) ---\n{}", stderr);

    let count = ch_row_count(CH_PORT, &ch_fq).await;
    assert!(
        count >= 3,
        "expected at least 3 rows in ClickHouse after initial sync, got {}",
        count
    );

    let client = pg_client(PG_SOURCE_PORT).await;
    pg_execute(
        &client,
        &format!(
            "INSERT INTO {} (id, name, city) VALUES (4, 'Dave', 'Tokyo')",
            table
        ),
    )
    .await;
    pg_execute(
        &client,
        &format!("UPDATE {} SET name = 'Alice_Updated' WHERE id = 1", table),
    )
    .await;
    pg_execute(&client, &format!("DELETE FROM {} WHERE id = 3", table)).await;

    let harness2 = CdcE2eHarness::new("ch_life", &config);
    let stderr2 = harness2.run_sync_with_timeout(Duration::from_secs(25));
    eprintln!("--- skippr-el stderr (lifecycle sync 2) ---\n{}", stderr2);

    let final_count = ch_row_count(CH_PORT, &ch_fq).await;
    assert_eq!(
        final_count, 3,
        "expected 3 rows after insert/update/delete replay, got {}",
        final_count
    );

    let n1 = ch_query(
        CH_PORT,
        &format!(r#"SELECT name FROM {} WHERE id = 1"#, ch_fq),
    )
    .await;
    assert!(
        n1.trim().contains("Alice_Updated"),
        "row 1 should reflect UPDATE, got {:?}",
        n1
    );

    let n2 = ch_query(
        CH_PORT,
        &format!(r#"SELECT name FROM {} WHERE id = 2"#, ch_fq),
    )
    .await;
    assert!(
        n2.trim().contains("Bob"),
        "row 2 should remain, got {:?}",
        n2
    );

    let n4 = ch_query(
        CH_PORT,
        &format!(r#"SELECT name FROM {} WHERE id = 4"#, ch_fq),
    )
    .await;
    assert!(
        n4.trim().contains("Dave"),
        "row 4 should exist after INSERT, got {:?}",
        n4
    );

    let c3 = ch_query(
        CH_PORT,
        &format!(r#"SELECT count() FROM {} WHERE id = 3"#, ch_fq),
    )
    .await;
    assert_eq!(
        c3.trim(),
        "0",
        "row 3 should be deleted in ClickHouse, got {:?}",
        c3
    );

    cleanup_source_replication("skippr_slot", "skippr_publication").await;
}

#[tokio::test]
#[ignore]
async fn e2e_postgres_to_clickhouse_tombstone_exists() {
    let table = unique_table("ch_tomb");

    cleanup_source_replication("skippr_slot", "skippr_publication").await;
    setup_pg_source(&table).await;
    seed_pg_rows(&table, &[(1, "Alice", "London"), (2, "Bob", "Paris")]).await;

    ch_exec(
        CH_PORT,
        &format!("DROP TABLE IF EXISTS \"default\".\"postgres.{}\"", table),
    )
    .await;

    let config =
        pg_to_clickhouse_cdc_config_yaml(PG_SOURCE_PORT, CH_PORT, "ch_tomb", &[&table], &["id"]);

    let harness = CdcE2eHarness::new("ch_tomb", &config);
    let stderr = harness.run_sync_with_timeout(Duration::from_secs(25));
    eprintln!("--- skippr-el stderr (tombstone) ---\n{}", stderr);

    let tables = ch_query(CH_PORT, "SHOW TABLES FROM default LIKE '%tombstones%'").await;
    assert!(
        !tables.trim().is_empty(),
        "expected at least one tombstone table, got {:?}",
        tables
    );
    assert!(
        tables.to_lowercase().contains("tombstone"),
        "SHOW TABLES should list tombstone-related table, output: {:?}",
        tables
    );

    cleanup_source_replication("skippr_slot", "skippr_publication").await;
}
