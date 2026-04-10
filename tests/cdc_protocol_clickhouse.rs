/// CDC Protocol Tests: ClickHouse  (secondary coverage)
///
/// Low-level tests that validate ClickHouse CDC SQL generation and apply.
/// These are NOT the CDC acceptance gate — see `cdc_e2e_clickhouse` for
/// the full-pipeline end-to-end suite.
///
/// Requires Docker service `clickhouse`.
///
/// Run: `cargo test --test cdc_protocol_clickhouse -- --ignored`
use skippr::runtime_test_cdc_apply::cdc_apply::{
    ddl_add_order_token_column, ddl_create_tombstone_table, delete_if_newer_sql,
    tombstone_table_name, upsert_if_newer_sql, SqlDialect,
};

async fn execute_sql(_client: &reqwest::Client, sql: &str) -> Result<String, String> {
    let output = std::process::Command::new("docker")
        .args([
            "exec",
            "skipprd_clickhouse_1",
            "clickhouse-client",
            "--query",
            sql,
        ])
        .output()
        .map_err(|e| e.to_string())?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if !output.status.success() {
        return Err(if stderr.trim().is_empty() {
            stdout
        } else {
            stderr
        });
    }
    if stdout.contains("Exception") || stdout.contains("DB::Exception") {
        return Err(stdout);
    }
    Ok(stdout)
}

/// Execute a multi-statement SQL string (`;`-separated) one statement at a time.
/// ClickHouse HTTP API accepts a single statement per request.
async fn execute_multi_sql(client: &reqwest::Client, sql: &str) -> Result<(), String> {
    for stmt in sql.split(';') {
        let trimmed = stmt.trim();
        if trimmed.is_empty() {
            continue;
        }
        execute_sql(client, trimmed).await?;
    }
    Ok(())
}

// -----------------------------------------------------------------------
// 1. DDL: order token column (idempotent) + tombstone table creation
// -----------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn cdc_clickhouse_ddl_order_token_and_tombstone() {
    let client = reqwest::Client::new();

    execute_sql(&client, "DROP TABLE IF EXISTS cdc_ch_test")
        .await
        .unwrap();
    execute_sql(
        &client,
        r#"DROP TABLE IF EXISTS "_skippr_tombstones_cdc_ch_test""#,
    )
    .await
    .unwrap();

    execute_sql(
        &client,
        r#"CREATE TABLE IF NOT EXISTS cdc_ch_test (id UInt64, name String, "_skippr_order_token" String) ENGINE = MergeTree() ORDER BY id"#,
    )
    .await
    .unwrap();

    // Should be idempotent — column already exists
    let ddl1 = ddl_add_order_token_column(SqlDialect::ClickHouse, "cdc_ch_test");
    execute_sql(&client, &ddl1).await.unwrap();

    let ts_table = tombstone_table_name("cdc_ch_test");
    let ddl2 = ddl_create_tombstone_table(
        SqlDialect::ClickHouse,
        &ts_table,
        &[("id".to_string(), "UInt64".to_string())],
    );
    execute_sql(&client, &ddl2).await.unwrap();

    let result = execute_sql(&client, r#"EXISTS TABLE "_skippr_tombstones_cdc_ch_test""#)
        .await
        .unwrap();
    assert_eq!(result.trim(), "1", "tombstone table should exist");

    execute_sql(&client, "DROP TABLE IF EXISTS cdc_ch_test")
        .await
        .unwrap();
    execute_sql(
        &client,
        r#"DROP TABLE IF EXISTS "_skippr_tombstones_cdc_ch_test""#,
    )
    .await
    .unwrap();
}

// -----------------------------------------------------------------------
// 2. Upsert-if-newer: stale write rejection
// -----------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn cdc_clickhouse_upsert_if_newer() {
    let client = reqwest::Client::new();
    let table = "cdc_ch_stale";
    let ts_table = tombstone_table_name(table);

    execute_sql(&client, &format!("DROP TABLE IF EXISTS {table}"))
        .await
        .unwrap();
    execute_sql(&client, &format!("DROP TABLE IF EXISTS {ts_table}"))
        .await
        .unwrap();

    execute_sql(
        &client,
        &format!(
            r#"CREATE TABLE {table} (id UInt64, name String, "_skippr_order_token" String) ENGINE = MergeTree() ORDER BY id"#
        ),
    )
    .await
    .unwrap();

    let ddl = ddl_create_tombstone_table(
        SqlDialect::ClickHouse,
        &ts_table,
        &[("id".to_string(), "UInt64".to_string())],
    );
    execute_sql(&client, &ddl).await.unwrap();

    // Insert with order_token = 0002
    let sql1 = upsert_if_newer_sql(
        SqlDialect::ClickHouse,
        table,
        &ts_table,
        &[
            "\"id\"".to_string(),
            "\"name\"".to_string(),
            "\"_skippr_order_token\"".to_string(),
        ],
        &[
            "1".to_string(),
            "'Alice'".to_string(),
            "'0000000000000002'".to_string(),
        ],
        &["\"id\"".to_string()],
        "0000000000000002",
    );
    execute_multi_sql(&client, &sql1).await.unwrap();
    execute_sql(&client, &format!("OPTIMIZE TABLE {table} FINAL"))
        .await
        .unwrap();

    let result = execute_sql(&client, &format!("SELECT name FROM {table} WHERE id = 1"))
        .await
        .unwrap();
    assert_eq!(result.trim(), "Alice");

    // Stale update with order_token = 0001 (should be rejected)
    let sql2 = upsert_if_newer_sql(
        SqlDialect::ClickHouse,
        table,
        &ts_table,
        &[
            "\"id\"".to_string(),
            "\"name\"".to_string(),
            "\"_skippr_order_token\"".to_string(),
        ],
        &[
            "1".to_string(),
            "'Stale'".to_string(),
            "'0000000000000001'".to_string(),
        ],
        &["\"id\"".to_string()],
        "0000000000000001",
    );
    execute_multi_sql(&client, &sql2).await.unwrap();
    execute_sql(&client, &format!("OPTIMIZE TABLE {table} FINAL"))
        .await
        .unwrap();

    let result = execute_sql(&client, &format!("SELECT name FROM {table} WHERE id = 1"))
        .await
        .unwrap();
    assert_eq!(
        result.trim(),
        "Alice",
        "stale write should have been rejected"
    );

    // Newer update with order_token = 0003 (should succeed)
    let sql3 = upsert_if_newer_sql(
        SqlDialect::ClickHouse,
        table,
        &ts_table,
        &[
            "\"id\"".to_string(),
            "\"name\"".to_string(),
            "\"_skippr_order_token\"".to_string(),
        ],
        &[
            "1".to_string(),
            "'Bob'".to_string(),
            "'0000000000000003'".to_string(),
        ],
        &["\"id\"".to_string()],
        "0000000000000003",
    );
    execute_multi_sql(&client, &sql3).await.unwrap();
    execute_sql(&client, &format!("OPTIMIZE TABLE {table} FINAL"))
        .await
        .unwrap();

    let result = execute_sql(&client, &format!("SELECT name FROM {table} WHERE id = 1"))
        .await
        .unwrap();
    assert_eq!(result.trim(), "Bob", "newer write should have succeeded");

    execute_sql(&client, &format!("DROP TABLE IF EXISTS {table}"))
        .await
        .unwrap();
    execute_sql(&client, &format!("DROP TABLE IF EXISTS {ts_table}"))
        .await
        .unwrap();
}

// -----------------------------------------------------------------------
// 3. Delete-if-newer + tombstone prevents stale resurrect
// -----------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn cdc_clickhouse_delete_if_newer_with_tombstone() {
    let client = reqwest::Client::new();
    let table = "cdc_ch_del";
    let ts_table = tombstone_table_name(table);

    execute_sql(&client, &format!("DROP TABLE IF EXISTS {table}"))
        .await
        .unwrap();
    execute_sql(&client, &format!("DROP TABLE IF EXISTS {ts_table}"))
        .await
        .unwrap();

    execute_sql(
        &client,
        &format!(
            r#"CREATE TABLE {table} (id UInt64, name String, "_skippr_order_token" String) ENGINE = MergeTree() ORDER BY id"#
        ),
    )
    .await
    .unwrap();

    let ddl = ddl_create_tombstone_table(
        SqlDialect::ClickHouse,
        &ts_table,
        &[("id".to_string(), "UInt64".to_string())],
    );
    execute_sql(&client, &ddl).await.unwrap();

    // Insert row with token 0002
    let sql_insert = upsert_if_newer_sql(
        SqlDialect::ClickHouse,
        table,
        &ts_table,
        &[
            "\"id\"".to_string(),
            "\"name\"".to_string(),
            "\"_skippr_order_token\"".to_string(),
        ],
        &[
            "1".to_string(),
            "'Alice'".to_string(),
            "'0000000000000002'".to_string(),
        ],
        &["\"id\"".to_string()],
        "0000000000000002",
    );
    execute_multi_sql(&client, &sql_insert).await.unwrap();
    execute_sql(&client, &format!("OPTIMIZE TABLE {table} FINAL"))
        .await
        .unwrap();

    // Delete with token 0003
    let sql_delete = delete_if_newer_sql(
        SqlDialect::ClickHouse,
        table,
        &ts_table,
        &["\"id\"".to_string()],
        &["1".to_string()],
        &["UInt64".to_string()],
        "0000000000000003",
    );
    execute_multi_sql(&client, &sql_delete).await.unwrap();
    execute_sql(&client, &format!("OPTIMIZE TABLE {table} FINAL"))
        .await
        .unwrap();
    execute_sql(&client, &format!("OPTIMIZE TABLE {ts_table} FINAL"))
        .await
        .unwrap();

    let result = execute_sql(
        &client,
        &format!("SELECT count() FROM {table} WHERE id = 1"),
    )
    .await
    .unwrap();
    assert_eq!(result.trim(), "0", "row should be deleted");

    let result = execute_sql(
        &client,
        &format!(r#"SELECT count() FROM {ts_table} WHERE "id" = 1"#),
    )
    .await
    .unwrap();
    assert_eq!(result.trim(), "1", "tombstone should exist");

    // Stale insert with token 0002 should be blocked by tombstone
    let sql_stale = upsert_if_newer_sql(
        SqlDialect::ClickHouse,
        table,
        &ts_table,
        &[
            "\"id\"".to_string(),
            "\"name\"".to_string(),
            "\"_skippr_order_token\"".to_string(),
        ],
        &[
            "1".to_string(),
            "'Zombie'".to_string(),
            "'0000000000000002'".to_string(),
        ],
        &["\"id\"".to_string()],
        "0000000000000002",
    );
    execute_multi_sql(&client, &sql_stale).await.unwrap();
    execute_sql(&client, &format!("OPTIMIZE TABLE {table} FINAL"))
        .await
        .unwrap();

    let result = execute_sql(
        &client,
        &format!("SELECT count() FROM {table} WHERE id = 1"),
    )
    .await
    .unwrap();
    assert_eq!(
        result.trim(),
        "0",
        "stale insert should be blocked by tombstone"
    );

    execute_sql(&client, &format!("DROP TABLE IF EXISTS {table}"))
        .await
        .unwrap();
    execute_sql(&client, &format!("DROP TABLE IF EXISTS {ts_table}"))
        .await
        .unwrap();
}

// -----------------------------------------------------------------------
// 4. Replay idempotency: replaying the same inserts is safe
// -----------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn cdc_clickhouse_replay_idempotency() {
    let client = reqwest::Client::new();
    let table = "cdc_ch_replay";
    let ts_table = tombstone_table_name(table);

    execute_sql(&client, &format!("DROP TABLE IF EXISTS {table}"))
        .await
        .unwrap();
    execute_sql(&client, &format!("DROP TABLE IF EXISTS {ts_table}"))
        .await
        .unwrap();

    execute_sql(
        &client,
        &format!(
            r#"CREATE TABLE {table} (id UInt64, val String, "_skippr_order_token" String) ENGINE = MergeTree() ORDER BY id"#
        ),
    )
    .await
    .unwrap();

    let ddl = ddl_create_tombstone_table(
        SqlDialect::ClickHouse,
        &ts_table,
        &[("id".to_string(), "UInt64".to_string())],
    );
    execute_sql(&client, &ddl).await.unwrap();

    let sql = upsert_if_newer_sql(
        SqlDialect::ClickHouse,
        table,
        &ts_table,
        &[
            "\"id\"".to_string(),
            "\"val\"".to_string(),
            "\"_skippr_order_token\"".to_string(),
        ],
        &[
            "1".to_string(),
            "'first'".to_string(),
            "'0000000000000001'".to_string(),
        ],
        &["\"id\"".to_string()],
        "0000000000000001",
    );

    // Execute twice — should be idempotent
    execute_multi_sql(&client, &sql).await.unwrap();
    execute_sql(&client, &format!("OPTIMIZE TABLE {table} FINAL"))
        .await
        .unwrap();
    execute_multi_sql(&client, &sql).await.unwrap();
    execute_sql(&client, &format!("OPTIMIZE TABLE {table} FINAL"))
        .await
        .unwrap();

    let result = execute_sql(&client, &format!("SELECT count() FROM {table}"))
        .await
        .unwrap();
    assert_eq!(result.trim(), "1");

    let result = execute_sql(&client, &format!("SELECT val FROM {table} WHERE id = 1"))
        .await
        .unwrap();
    assert_eq!(result.trim(), "first");

    execute_sql(&client, &format!("DROP TABLE IF EXISTS {table}"))
        .await
        .unwrap();
    execute_sql(&client, &format!("DROP TABLE IF EXISTS {ts_table}"))
        .await
        .unwrap();
}
