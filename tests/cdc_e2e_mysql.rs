/// CDC End-to-End: MySQL source → Postgres sink
///
/// Requires Docker services: `mysql` (port 13306) and `postgres-target` (port 15433).
///
///   cargo test --test cdc_e2e_mysql -- --ignored
mod support;

use std::time::Duration;
use support::cdc_e2e::*;

const MYSQL_PORT: u16 = 13306;
const TARGET_PORT: u16 = 15433;

fn unique_table(base: &str) -> String {
    format!("{}_{}", base, rand::random::<u32>())
}

/// MySQL CDC namespaces tables on the sink as `mysql.{source_table_name}` (see `e2e_mysql_cdc_snapshot_reaches_target`).
fn pg_target_table_name(mysql_table: &str) -> String {
    format!("mysql.{}", mysql_table)
}

async fn setup_mysql_table(pool: &mysql_async::Pool, table: &str) {
    mysql_exec(
        pool,
        &format!(
            "DROP TABLE IF EXISTS {}; CREATE TABLE {} (id BIGINT PRIMARY KEY, name VARCHAR(255), city VARCHAR(255))",
            table, table
        ),
    )
    .await;
}

async fn seed_mysql_rows(pool: &mysql_async::Pool, table: &str, rows: &[(i64, &str, &str)]) {
    for (id, name, city) in rows {
        mysql_exec(
            pool,
            &format!(
                "INSERT INTO {} (id, name, city) VALUES ({}, '{}', '{}')",
                table, id, name, city
            ),
        )
        .await;
    }
}

#[tokio::test]
#[ignore]
async fn e2e_mysql_cdc_snapshot_reaches_target() {
    let table = unique_table("cdc_my");
    let pool = mysql_pool(MYSQL_PORT).await;

    setup_mysql_table(&pool, &table).await;
    seed_mysql_rows(
        &pool,
        &table,
        &[(1, "Alice", "London"), (2, "Bob", "Paris")],
    )
    .await;

    let target_table = pg_target_table_name(&table);
    let target = pg_client(TARGET_PORT).await;
    let _ = target
        .batch_execute(&format!(
            "DROP TABLE IF EXISTS \"public\".\"{}\" CASCADE",
            target_table
        ))
        .await;

    let config =
        mysql_to_pg_cdc_config_yaml(MYSQL_PORT, TARGET_PORT, "cdc_my_snap", &table, &["id"]);

    let harness = CdcE2eHarness::new("cdc_my_snap", &config);
    let stderr = harness.run_sync_with_timeout(Duration::from_secs(20));
    eprintln!("--- skippr-el stderr ---\n{}", stderr);

    let fq = format!("\"public\".\"{}\"", target_table);
    let count = pg_row_count(&target, &fq).await;
    assert!(count >= 2, "expected at least 2 rows, got {}", count);

    let has_token = pg_column_exists(&target, &target_table, "_skippr_order_token").await;
    assert!(has_token, "target should have _skippr_order_token");

    pool.disconnect().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn e2e_mysql_cdc_insert_update_delete_lifecycle() {
    let pool = mysql_pool(MYSQL_PORT).await;
    mysql_exec(
        &pool,
        "DROP TABLE IF EXISTS cdc_life; CREATE TABLE cdc_life (id BIGINT PRIMARY KEY, name VARCHAR(255), city VARCHAR(255))",
    )
    .await;
    seed_mysql_rows(
        &pool,
        "cdc_life",
        &[
            (1, "Alice", "London"),
            (2, "Bob", "Paris"),
            (3, "Carol", "NYC"),
        ],
    )
    .await;

    let target_table = pg_target_table_name("cdc_life");
    let target = pg_client(TARGET_PORT).await;
    let _ = target
        .batch_execute(&format!(
            "DROP TABLE IF EXISTS \"public\".\"{}\" CASCADE",
            target_table
        ))
        .await;

    let config =
        mysql_to_pg_cdc_config_yaml(MYSQL_PORT, TARGET_PORT, "mysql_life", "cdc_life", &["id"]);

    let harness = CdcE2eHarness::new("mysql_life", &config);
    let stderr = harness.run_sync_with_timeout(Duration::from_secs(25));
    eprintln!("--- skippr-el stderr (lifecycle sync 1) ---\n{}", stderr);

    let fq = format!("\"public\".\"{}\"", target_table);
    let count = pg_row_count(&target, &fq).await;
    assert!(
        count >= 3,
        "expected at least 3 rows after initial sync, got {}",
        count
    );

    let has_token = pg_column_exists(&target, &target_table, "_skippr_order_token").await;
    assert!(has_token, "target should have _skippr_order_token");

    mysql_exec_update(&pool, "UPDATE cdc_life SET name='Alice_Updated' WHERE id=1").await;
    mysql_exec_delete(&pool, "DELETE FROM cdc_life WHERE id=3").await;

    let harness2 = CdcE2eHarness::new("mysql_life", &config);
    let stderr2 = harness2.run_sync_with_timeout(Duration::from_secs(25));
    eprintln!("--- skippr-el stderr (lifecycle sync 2) ---\n{}", stderr2);

    assert!(
        pg_row_exists(&target, &fq, "id = 1 AND name = 'Alice_Updated'").await,
        "row 1 should reflect UPDATE",
    );
    assert!(
        pg_row_exists(&target, &fq, "id = 2 AND name = 'Bob'").await,
        "row 2 should remain",
    );
    assert!(
        !pg_row_exists(&target, &fq, "id = 3").await,
        "row 3 should be deleted",
    );

    pool.disconnect().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn e2e_mysql_cdc_replay_is_idempotent() {
    let pool = mysql_pool(MYSQL_PORT).await;
    mysql_exec(
        &pool,
        "DROP TABLE IF EXISTS cdc_replay; CREATE TABLE cdc_replay (id BIGINT PRIMARY KEY, name VARCHAR(255), city VARCHAR(255))",
    )
    .await;
    seed_mysql_rows(
        &pool,
        "cdc_replay",
        &[(1, "Alice", "London"), (2, "Bob", "Paris")],
    )
    .await;

    let target_table = pg_target_table_name("cdc_replay");
    let target = pg_client(TARGET_PORT).await;
    let _ = target
        .batch_execute(&format!(
            "DROP TABLE IF EXISTS \"public\".\"{}\" CASCADE",
            target_table
        ))
        .await;

    let config = mysql_to_pg_cdc_config_yaml(
        MYSQL_PORT,
        TARGET_PORT,
        "mysql_replay",
        "cdc_replay",
        &["id"],
    );

    let harness1 = CdcE2eHarness::new("mysql_replay", &config);
    let stderr1 = harness1.run_sync_with_timeout(Duration::from_secs(25));
    eprintln!("--- skippr-el stderr (replay 1) ---\n{}", stderr1);

    let fq = format!("\"public\".\"{}\"", target_table);
    let count1 = pg_row_count(&target, &fq).await;
    assert_eq!(
        count1, 2,
        "expected 2 rows after first sync, got {}",
        count1
    );

    let harness2 = CdcE2eHarness::new("mysql_replay", &config);
    let stderr2 = harness2.run_sync_with_timeout(Duration::from_secs(25));
    eprintln!("--- skippr-el stderr (replay 2) ---\n{}", stderr2);

    let count2 = pg_row_count(&target, &fq).await;
    assert_eq!(
        count2, count1,
        "replay should not duplicate rows: first count {}, second {}",
        count1, count2
    );

    pool.disconnect().await.unwrap();
}
