/// CDC End-to-End: Postgres source → Postgres sink
///
/// These tests exercise the **full pipeline** through the real `skippr-el`
/// binary: source plugin → ingest → WAL → compactor → sink plugin → target DB.
///
/// Requires Docker services: `postgres` (port 15432) and `postgres-target`
/// (port 15433).  Run with:
///
///   cargo test --test cdc_e2e_postgres -- --ignored
mod support;

use std::time::Duration;
use support::cdc_e2e::*;

const SOURCE_PORT: u16 = 15432;
const TARGET_PORT: u16 = 15433;

/// Unique table name per test to avoid cross-test interference.
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

// ---------------------------------------------------------------------------
// Test 1: Snapshot CDC data reaches the target with _skippr_order_token
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_postgres_cdc_snapshot_populates_order_token() {
    let table = unique_table("cdc_snap");

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

    let target_table_name = format!("postgres.{}", table);
    cleanup_target(&table).await;

    let config = pg_cdc_config_yaml(SOURCE_PORT, TARGET_PORT, "cdc_pg_snap", &[&table], &["id"]);
    let harness = CdcE2eHarness::new("cdc_pg_snap", &config);

    let stderr = harness.run_sync_with_timeout(Duration::from_secs(20));
    eprintln!("--- skippr-el stderr ---\n{}", stderr);

    let target = pg_client(TARGET_PORT).await;

    let fq = format!("\"public\".\"{}\"", target_table_name);

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
        "target table should have _skippr_order_token column (CDC was applied, not append)"
    );

    let non_null_tokens: i64 = target
        .query_one(
            &format!(
                "SELECT COUNT(*)::bigint FROM {} WHERE \"_skippr_order_token\" IS NOT NULL",
                fq
            ),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(
        non_null_tokens >= 3,
        "all snapshot rows should have non-null order tokens, got {} non-null",
        non_null_tokens
    );

    let tombstone_table = format!("_skippr_tombstones_{}", target_table_name);
    let has_tombstone = pg_table_exists(&target, &tombstone_table).await;
    assert!(
        has_tombstone,
        "tombstone table {} should exist after CDC sync",
        tombstone_table
    );

    cleanup_source_replication("skippr_slot", "skippr_publication").await;
}

// ---------------------------------------------------------------------------
// Test 2: Replay idempotency — running the pipeline twice with the same
//         snapshot data must not duplicate rows or regress order tokens.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_postgres_cdc_replay_is_idempotent() {
    let table = unique_table("cdc_replay");

    cleanup_source_replication("skippr_slot", "skippr_publication").await;
    setup_source_table(&table).await;
    seed_source_rows(&table, &[(10, "Replay1", "NYC"), (20, "Replay2", "LA")]).await;

    let target_table_name = format!("postgres.{}", table);
    cleanup_target(&table).await;

    let config = pg_cdc_config_yaml(
        SOURCE_PORT,
        TARGET_PORT,
        "cdc_pg_replay",
        &[&table],
        &["id"],
    );

    // First run
    {
        let harness = CdcE2eHarness::new("cdc_pg_replay", &config);
        harness.run_sync_with_timeout(Duration::from_secs(15));
    }

    cleanup_source_replication("skippr_slot", "skippr_publication").await;

    let target = pg_client(TARGET_PORT).await;
    let fq = format!("\"public\".\"{}\"", target_table_name);
    let count_after_first = pg_row_count(&target, &fq).await;

    // Second run (same source data, fresh slot)
    {
        let harness = CdcE2eHarness::new("cdc_pg_replay", &config);
        harness.run_sync_with_timeout(Duration::from_secs(15));
    }

    cleanup_source_replication("skippr_slot", "skippr_publication").await;

    let count_after_second = pg_row_count(&target, &fq).await;

    assert_eq!(
        count_after_first, count_after_second,
        "replay should be idempotent: first={} second={}",
        count_after_first, count_after_second
    );
}

// ---------------------------------------------------------------------------
// Test 3: Without CDC config, target should NOT have _skippr_order_token
//         (proves that the CDC path is taken only when configured).
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_postgres_append_mode_has_no_order_token() {
    let table = unique_table("cdc_append");

    cleanup_source_replication("skippr_slot", "skippr_publication").await;
    setup_source_table(&table).await;
    seed_source_rows(&table, &[(100, "AppendOnly", "Nowhere")]).await;

    let target_table_name = format!("postgres.{}", table);
    cleanup_target(&table).await;

    let config = format!(
        r#"data_sources:
  pg_source:
    Postgres:
      host: 127.0.0.1
      port: {SOURCE_PORT}
      user: postgres
      password: testpass
      database: skippr_test
      tables: ["{table}"]

data_sinks:
  pg_target:
    Postgres:
      host: 127.0.0.1
      port: {TARGET_PORT}
      user: postgres
      password: testpass
      database: skippr_test
      schema: public

pipelines:
  append_pg:
    data_source: data_sources.pg_source
    data_sink: data_sinks.pg_target
"#
    );
    let harness = CdcE2eHarness::new("append_pg", &config);
    harness.run_sync_with_timeout(Duration::from_secs(15));

    let target = pg_client(TARGET_PORT).await;
    let has_order_token =
        pg_column_exists(&target, &target_table_name, "_skippr_order_token").await;
    assert!(
        !has_order_token,
        "append-mode target should NOT have _skippr_order_token column"
    );
}

// ---------------------------------------------------------------------------
// Test 4: INSERT / UPDATE / DELETE on source, then full re-sync (fresh slot)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_postgres_cdc_insert_update_delete_lifecycle() {
    let table = unique_table("cdc_lifecycle");

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

    let target_table_name = format!("postgres.{}", table);
    cleanup_target(&table).await;

    let config = pg_cdc_config_yaml(
        SOURCE_PORT,
        TARGET_PORT,
        "cdc_pg_lifecycle",
        &[&table],
        &["id"],
    );

    {
        let harness = CdcE2eHarness::new("cdc_pg_lifecycle", &config);
        let stderr = harness.run_sync_with_timeout(Duration::from_secs(20));
        eprintln!("--- skippr-el stderr (first) ---\n{}", stderr);
    }

    let target = pg_client(TARGET_PORT).await;
    let fq = format!("\"public\".\"{}\"", target_table_name);
    let count_first = pg_row_count(&target, &fq).await;
    assert_eq!(
        count_first, 3,
        "expected 3 rows in target after first sync, got {}",
        count_first
    );

    let source = pg_client(SOURCE_PORT).await;
    pg_execute(
        &source,
        &format!("UPDATE {} SET name = 'Bob_Updated' WHERE id = 2", table),
    )
    .await;
    pg_execute(&source, &format!("DELETE FROM {} WHERE id = 3", table)).await;

    cleanup_source_replication("skippr_slot", "skippr_publication").await;

    {
        let harness = CdcE2eHarness::new("cdc_pg_lifecycle", &config);
        let stderr = harness.run_sync_with_timeout(Duration::from_secs(20));
        eprintln!("--- skippr-el stderr (second) ---\n{}", stderr);
    }

    cleanup_source_replication("skippr_slot", "skippr_publication").await;

    let target = pg_client(TARGET_PORT).await;
    pg_wait_for_rows(&target, &fq, 2, Duration::from_secs(30)).await;
    assert!(
        pg_row_exists(&target, &fq, "id = 2 AND name = 'Bob_Updated'").await,
        "id=2 should reflect UPDATE to Bob_Updated after second snapshot"
    );
    assert!(
        !pg_row_exists(&target, &fq, "id = 3").await,
        "deleted id=3 should not appear after second snapshot"
    );
    assert!(
        pg_row_exists(&target, &fq, "id = 1 AND name = 'Alice'").await,
        "id=1 row should still be present"
    );
}

// ---------------------------------------------------------------------------
// Test 5: Stale write rejection — replay with fresh slot does not duplicate
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_postgres_cdc_stale_write_rejected() {
    let table = unique_table("cdc_stale");

    cleanup_source_replication("skippr_slot", "skippr_publication").await;
    setup_source_table(&table).await;
    seed_source_rows(&table, &[(1, "One", "A"), (2, "Two", "B")]).await;

    let target_table_name = format!("postgres.{}", table);
    cleanup_target(&table).await;

    let config = pg_cdc_config_yaml(SOURCE_PORT, TARGET_PORT, "cdc_pg_stale", &[&table], &["id"]);

    {
        let harness = CdcE2eHarness::new("cdc_pg_stale", &config);
        harness.run_sync_with_timeout(Duration::from_secs(20));
    }

    cleanup_source_replication("skippr_slot", "skippr_publication").await;

    let target = pg_client(TARGET_PORT).await;
    let fq = format!("\"public\".\"{}\"", target_table_name);
    let count_after_first = pg_row_count(&target, &fq).await;
    assert_eq!(count_after_first, 2, "expected 2 rows after first sync");

    let tokens = pg_order_tokens(&target, &fq).await;
    assert!(
        tokens.len() >= 2,
        "expected order tokens on both rows, got {}",
        tokens.len()
    );

    {
        let harness = CdcE2eHarness::new("cdc_pg_stale", &config);
        harness.run_sync_with_timeout(Duration::from_secs(20));
    }

    cleanup_source_replication("skippr_slot", "skippr_publication").await;

    let count_after_second = pg_row_count(&target, &fq).await;
    assert_eq!(
        count_after_first, count_after_second,
        "second run with fresh slot should not add duplicate rows: first={} second={}",
        count_after_first, count_after_second
    );
}

// ---------------------------------------------------------------------------
// Test 6: Tombstone — deleted source row does not come back on full re-sync
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_postgres_cdc_tombstone_blocks_resurrect() {
    let table = unique_table("cdc_tomb");

    cleanup_source_replication("skippr_slot", "skippr_publication").await;
    setup_source_table(&table).await;
    seed_source_rows(&table, &[(1, "Alice", "London"), (2, "Bob", "Paris")]).await;

    let target_table_name = format!("postgres.{}", table);
    cleanup_target(&table).await;

    let config = pg_cdc_config_yaml(
        SOURCE_PORT,
        TARGET_PORT,
        "cdc_pg_tombstone",
        &[&table],
        &["id"],
    );

    {
        let harness = CdcE2eHarness::new("cdc_pg_tombstone", &config);
        harness.run_sync_with_timeout(Duration::from_secs(20));
    }

    let target = pg_client(TARGET_PORT).await;
    let fq = format!("\"public\".\"{}\"", target_table_name);
    assert_eq!(pg_row_count(&target, &fq).await, 2);

    let source = pg_client(SOURCE_PORT).await;
    pg_execute(&source, &format!("DELETE FROM {} WHERE id = 1", table)).await;

    cleanup_source_replication("skippr_slot", "skippr_publication").await;

    {
        let harness = CdcE2eHarness::new("cdc_pg_tombstone", &config);
        harness.run_sync_with_timeout(Duration::from_secs(20));
    }

    cleanup_source_replication("skippr_slot", "skippr_publication").await;

    let target = pg_client(TARGET_PORT).await;
    pg_wait_for_rows(&target, &fq, 1, Duration::from_secs(30)).await;
    assert_eq!(pg_row_count(&target, &fq).await, 1);
    assert!(
        !pg_row_exists(&target, &fq, "id = 1").await,
        "deleted id=1 should not be present after snapshot with one source row"
    );
    assert!(pg_row_exists(&target, &fq, "id = 2").await);
}

// ---------------------------------------------------------------------------
// Test 7: Resume — second sync on same harness picks up new inserts
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_postgres_cdc_resume_from_stored_lsn() {
    let table = unique_table("cdc_resume");

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

    let target_table_name = format!("postgres.{}", table);
    cleanup_target(&table).await;

    let config = pg_cdc_config_yaml(
        SOURCE_PORT,
        TARGET_PORT,
        "cdc_pg_resume",
        &[&table],
        &["id"],
    );

    let harness = CdcE2eHarness::new("cdc_pg_resume", &config);
    harness.run_sync_with_timeout(Duration::from_secs(20));

    let target = pg_client(TARGET_PORT).await;
    let fq = format!("\"public\".\"{}\"", target_table_name);
    pg_wait_for_rows(&target, &fq, 3, Duration::from_secs(30)).await;

    let source = pg_client(SOURCE_PORT).await;
    pg_execute(
        &source,
        &format!(
            "INSERT INTO {} (id, name, city) VALUES (4, 'Diana', 'Rome')",
            table
        ),
    )
    .await;

    harness.run_sync_with_timeout(Duration::from_secs(20));

    let target = pg_client(TARGET_PORT).await;
    pg_wait_for_rows(&target, &fq, 4, Duration::from_secs(30)).await;

    cleanup_source_replication("skippr_slot", "skippr_publication").await;
}
