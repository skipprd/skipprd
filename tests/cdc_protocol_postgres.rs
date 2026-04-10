/// CDC Protocol Tests: Postgres  (secondary coverage)
///
/// Low-level tests that validate Postgres CDC protocol details, WAL
/// metadata roundtrips, and generated SQL at the driver/SQL level.
/// These are NOT the CDC acceptance gate — see `cdc_e2e_postgres` for
/// the full-pipeline end-to-end suite.
///
/// Requires Docker services `postgres` and `postgres-target`.
///
/// Run: `cargo test --test cdc_protocol_postgres -- --ignored`
use std::collections::HashMap;
use std::time::SystemTime;

use skippr::buffer::segment_file::{PartitionKey, SegmentFile};
use skippr::plugins::cdc::{MutationKind, WalPartKind, WalPartMeta, WalRowMeta};
use skippr::runtime_test_cdc_apply::cdc_apply::{
    ddl_add_order_token_column, ddl_create_tombstone_table, tombstone_table_name, SqlDialect,
};

const SOURCE_CONN: &str =
    "host=localhost port=15432 user=postgres password=testpass dbname=skippr_test";
const TARGET_CONN: &str =
    "host=localhost port=15433 user=postgres password=testpass dbname=skippr_test";

async fn connect(conn_str: &str) -> tokio_postgres::Client {
    let (client, connection) = tokio_postgres::connect(conn_str, tokio_postgres::NoTls)
        .await
        .expect("connect failed");
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("connection error: {}", e);
        }
    });
    client
}

// -----------------------------------------------------------------------
// 1. WAL round-trip: write CDC metadata, read it back
// -----------------------------------------------------------------------

#[test]
#[ignore]
fn cdc_wal_roundtrip_with_postgres_metadata() {
    let dir = std::env::temp_dir().join(format!("skippr_cdc_pg_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let seg = SegmentFile::new(&dir, "pg_cdc_roundtrip").unwrap();

    let key = PartitionKey {
        sink_ref: "data_outputs.pg_target".to_string(),
        namespace: "users".to_string(),
        partition: "".to_string(),
        time: None,
        shard: "".to_string(),
    };

    let schema = arrow::datatypes::Schema::new(vec![
        arrow::datatypes::Field::new("id", arrow::datatypes::DataType::Int32, false),
        arrow::datatypes::Field::new("name", arrow::datatypes::DataType::Utf8, true),
    ]);
    let batch = arrow::record_batch::RecordBatch::try_new(
        std::sync::Arc::new(schema),
        vec![
            std::sync::Arc::new(arrow::array::Int32Array::from(vec![1, 2, 3])),
            std::sync::Arc::new(arrow::array::StringArray::from(vec!["a", "b", "c"])),
        ],
    )
    .unwrap();

    let mut batches: HashMap<PartitionKey, Vec<arrow::record_batch::RecordBatch>> = HashMap::new();
    batches.insert(key.clone(), vec![batch]);
    let mut parts_meta: HashMap<PartitionKey, (u64, SystemTime)> = HashMap::new();
    parts_meta.insert(key.clone(), (0, SystemTime::now()));
    let offsets = HashMap::new();

    let wal_meta = WalPartMeta {
        kind: WalPartKind::Cdc,
        row_count: 3,
        rows: vec![
            WalRowMeta {
                mutation: MutationKind::Insert,
                event_id: b"pg:lsn:0/1000000".to_vec(),
                order_token: 0x1000000u64.to_be_bytes().to_vec(),
            },
            WalRowMeta {
                mutation: MutationKind::Insert,
                event_id: b"pg:lsn:0/1000001".to_vec(),
                order_token: 0x1000001u64.to_be_bytes().to_vec(),
            },
            WalRowMeta {
                mutation: MutationKind::Update,
                event_id: b"pg:lsn:0/1000002".to_vec(),
                order_token: 0x1000002u64.to_be_bytes().to_vec(),
            },
        ],
    };
    let blob = bincode::serialize(&wal_meta).unwrap();
    let mut blobs: HashMap<PartitionKey, Vec<u8>> = HashMap::new();
    blobs.insert(key.clone(), blob);

    let (_meta, rows, _sha) = seg
        .write_snapshot(&offsets, &batches, &parts_meta, &blobs)
        .unwrap();
    assert_eq!(rows, 3);

    let mut f = std::fs::File::open(&seg.path).unwrap();
    let read_blobs = SegmentFile::read_part_meta_blobs_from_reader(&mut f).unwrap();
    let decoded: WalPartMeta = bincode::deserialize(read_blobs.get(&key).unwrap()).unwrap();
    assert_eq!(decoded.rows.len(), 3);
    assert_eq!(decoded.rows[2].mutation, MutationKind::Update);

    std::fs::remove_dir_all(&dir).ok();
}

// -----------------------------------------------------------------------
// 2. Source DDL: create publication + replication slot
// -----------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn cdc_source_can_create_publication_and_slot() {
    let client = connect(SOURCE_CONN).await;

    client
        .batch_execute("DROP TABLE IF EXISTS cdc_test_src; CREATE TABLE cdc_test_src (id SERIAL PRIMARY KEY, name TEXT);")
        .await
        .unwrap();
    client
        .batch_execute("DROP PUBLICATION IF EXISTS skippr_test_pub;")
        .await
        .unwrap();
    client
        .batch_execute("CREATE PUBLICATION skippr_test_pub FOR ALL TABLES;")
        .await
        .unwrap();

    // Drop slot if exists (cleanup from previous run)
    let _ = client
        .batch_execute("SELECT pg_drop_replication_slot('skippr_test_slot');")
        .await;
    let row = client
        .query_one(
            "SELECT slot_name, lsn::text FROM pg_create_logical_replication_slot('skippr_test_slot', 'pgoutput');",
            &[],
        )
        .await
        .unwrap();
    let lsn: String = row.get(1);
    assert!(!lsn.is_empty(), "LSN should be non-empty");

    // Cleanup
    let _ = client
        .batch_execute("SELECT pg_drop_replication_slot('skippr_test_slot');")
        .await;
    let _ = client
        .batch_execute("DROP PUBLICATION IF EXISTS skippr_test_pub;")
        .await;
    let _ = client
        .batch_execute("DROP TABLE IF EXISTS cdc_test_src;")
        .await;
}

// -----------------------------------------------------------------------
// 3. Sink DDL: order token + tombstone creation
// -----------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn cdc_sink_creates_order_token_and_tombstone() {
    let client = connect(TARGET_CONN).await;

    client
        .batch_execute(
            "DROP TABLE IF EXISTS \"_skippr_tombstones_cdc_target\"; \
             DROP TABLE IF EXISTS cdc_target; \
             CREATE TABLE cdc_target (id BIGINT PRIMARY KEY, name TEXT);",
        )
        .await
        .unwrap();

    let ddl1 = ddl_add_order_token_column(SqlDialect::Postgres, "cdc_target");
    client.batch_execute(&ddl1).await.unwrap();

    let ts_table = tombstone_table_name("cdc_target");
    let ddl2 = ddl_create_tombstone_table(
        SqlDialect::Postgres,
        &ts_table,
        &[("id".to_string(), "BIGINT".to_string())],
    );
    client.batch_execute(&ddl2).await.unwrap();

    // Verify columns exist
    let row = client
        .query_one(
            "SELECT column_name FROM information_schema.columns \
             WHERE table_name = 'cdc_target' AND column_name = '_skippr_order_token'",
            &[],
        )
        .await
        .unwrap();
    let col: String = row.get(0);
    assert_eq!(col, "_skippr_order_token");

    // Verify tombstone table exists
    let row = client
        .query_one(
            "SELECT count(*) FROM information_schema.tables WHERE table_name = '_skippr_tombstones_cdc_target'",
            &[],
        )
        .await
        .unwrap();
    let count: i64 = row.get(0);
    assert_eq!(count, 1);

    // Cleanup
    client
        .batch_execute("DROP TABLE IF EXISTS \"_skippr_tombstones_cdc_target\"; DROP TABLE IF EXISTS cdc_target;")
        .await
        .unwrap();
}

// -----------------------------------------------------------------------
// 4. Upsert-if-newer: stale write rejection
// -----------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn cdc_upsert_rejects_stale_write() {
    let client = connect(TARGET_CONN).await;

    client
        .batch_execute(
            "DROP TABLE IF EXISTS \"_skippr_tombstones_cdc_stale\"; \
             DROP TABLE IF EXISTS cdc_stale; \
             CREATE TABLE cdc_stale (id BIGINT PRIMARY KEY, name TEXT, \"_skippr_order_token\" BYTEA);",
        )
        .await
        .unwrap();

    let ts_table = tombstone_table_name("cdc_stale");
    let ddl = ddl_create_tombstone_table(
        SqlDialect::Postgres,
        &ts_table,
        &[("id".to_string(), "BIGINT".to_string())],
    );
    client.batch_execute(&ddl).await.unwrap();

    // Insert with order_token = 0002
    let sql1 = skippr::runtime_test_cdc_apply::cdc_apply::upsert_if_newer_sql(
        SqlDialect::Postgres,
        "cdc_stale",
        &ts_table,
        &[
            "\"id\"".to_string(),
            "\"name\"".to_string(),
            "\"_skippr_order_token\"".to_string(),
        ],
        &[
            "1".to_string(),
            "'Alice'".to_string(),
            "decode('0000000000000002', 'hex')".to_string(),
        ],
        &["\"id\"".to_string()],
        "0000000000000002",
    );
    client.batch_execute(&sql1).await.unwrap();

    let row = client
        .query_one("SELECT name FROM cdc_stale WHERE id = 1", &[])
        .await
        .unwrap();
    let name: String = row.get(0);
    assert_eq!(name, "Alice");

    // Attempt stale update with order_token = 0001 (should be rejected)
    let sql2 = skippr::runtime_test_cdc_apply::cdc_apply::upsert_if_newer_sql(
        SqlDialect::Postgres,
        "cdc_stale",
        &ts_table,
        &[
            "\"id\"".to_string(),
            "\"name\"".to_string(),
            "\"_skippr_order_token\"".to_string(),
        ],
        &[
            "1".to_string(),
            "'Stale'".to_string(),
            "decode('0000000000000001', 'hex')".to_string(),
        ],
        &["\"id\"".to_string()],
        "0000000000000001",
    );
    client.batch_execute(&sql2).await.unwrap();

    let row = client
        .query_one("SELECT name FROM cdc_stale WHERE id = 1", &[])
        .await
        .unwrap();
    let name: String = row.get(0);
    assert_eq!(name, "Alice", "stale write should have been rejected");

    // Newer update with order_token = 0003 (should succeed)
    let sql3 = skippr::runtime_test_cdc_apply::cdc_apply::upsert_if_newer_sql(
        SqlDialect::Postgres,
        "cdc_stale",
        &ts_table,
        &[
            "\"id\"".to_string(),
            "\"name\"".to_string(),
            "\"_skippr_order_token\"".to_string(),
        ],
        &[
            "1".to_string(),
            "'Bob'".to_string(),
            "decode('0000000000000003', 'hex')".to_string(),
        ],
        &["\"id\"".to_string()],
        "0000000000000003",
    );
    client.batch_execute(&sql3).await.unwrap();

    let row = client
        .query_one("SELECT name FROM cdc_stale WHERE id = 1", &[])
        .await
        .unwrap();
    let name: String = row.get(0);
    assert_eq!(name, "Bob", "newer write should have succeeded");

    // Cleanup
    client
        .batch_execute("DROP TABLE IF EXISTS \"_skippr_tombstones_cdc_stale\"; DROP TABLE IF EXISTS cdc_stale;")
        .await
        .unwrap();
}

// -----------------------------------------------------------------------
// 5. Delete-if-newer + tombstone prevents stale resurrect
// -----------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn cdc_delete_then_stale_insert_blocked_by_tombstone() {
    let client = connect(TARGET_CONN).await;

    client
        .batch_execute(
            "DROP TABLE IF EXISTS \"_skippr_tombstones_cdc_del\"; \
             DROP TABLE IF EXISTS cdc_del; \
             CREATE TABLE cdc_del (id BIGINT PRIMARY KEY, name TEXT, \"_skippr_order_token\" BYTEA);",
        )
        .await
        .unwrap();

    let ts_table = tombstone_table_name("cdc_del");
    let ddl = ddl_create_tombstone_table(
        SqlDialect::Postgres,
        &ts_table,
        &[("id".to_string(), "BIGINT".to_string())],
    );
    client.batch_execute(&ddl).await.unwrap();

    // Insert row with token 0002
    let sql_insert = skippr::runtime_test_cdc_apply::cdc_apply::upsert_if_newer_sql(
        SqlDialect::Postgres,
        "cdc_del",
        &ts_table,
        &[
            "\"id\"".to_string(),
            "\"name\"".to_string(),
            "\"_skippr_order_token\"".to_string(),
        ],
        &[
            "1".to_string(),
            "'Alice'".to_string(),
            "decode('0000000000000002', 'hex')".to_string(),
        ],
        &["\"id\"".to_string()],
        "0000000000000002",
    );
    client.batch_execute(&sql_insert).await.unwrap();

    // Delete with token 0003
    let sql_delete = skippr::runtime_test_cdc_apply::cdc_apply::delete_if_newer_sql(
        SqlDialect::Postgres,
        "cdc_del",
        &ts_table,
        &["\"id\"".to_string()],
        &["1".to_string()],
        &["BIGINT".to_string()],
        "0000000000000003",
    );
    client.batch_execute(&sql_delete).await.unwrap();

    let count: i64 = client
        .query_one("SELECT count(*) FROM cdc_del WHERE id = 1", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 0, "row should be deleted");

    // Tombstone should exist
    let ts_count: i64 = client
        .query_one(
            &format!("SELECT count(*) FROM {} WHERE \"id\" = 1", ts_table),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(ts_count, 1, "tombstone should exist");

    // Stale insert with token 0002 should be blocked by tombstone
    let sql_stale_insert = skippr::runtime_test_cdc_apply::cdc_apply::upsert_if_newer_sql(
        SqlDialect::Postgres,
        "cdc_del",
        &ts_table,
        &[
            "\"id\"".to_string(),
            "\"name\"".to_string(),
            "\"_skippr_order_token\"".to_string(),
        ],
        &[
            "1".to_string(),
            "'Zombie'".to_string(),
            "decode('0000000000000002', 'hex')".to_string(),
        ],
        &["\"id\"".to_string()],
        "0000000000000002",
    );
    client.batch_execute(&sql_stale_insert).await.unwrap();

    let count: i64 = client
        .query_one("SELECT count(*) FROM cdc_del WHERE id = 1", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 0, "stale insert should be blocked by tombstone");

    // Cleanup
    client
        .batch_execute(
            "DROP TABLE IF EXISTS \"_skippr_tombstones_cdc_del\"; DROP TABLE IF EXISTS cdc_del;",
        )
        .await
        .unwrap();
}

// -----------------------------------------------------------------------
// 6. Replay idempotency: replaying the same inserts is safe
// -----------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn cdc_replay_is_idempotent() {
    let client = connect(TARGET_CONN).await;

    client
        .batch_execute(
            "DROP TABLE IF EXISTS \"_skippr_tombstones_cdc_replay\"; \
             DROP TABLE IF EXISTS cdc_replay; \
             CREATE TABLE cdc_replay (id BIGINT PRIMARY KEY, val TEXT, \"_skippr_order_token\" BYTEA);",
        )
        .await
        .unwrap();

    let ts_table = tombstone_table_name("cdc_replay");
    let ddl = ddl_create_tombstone_table(
        SqlDialect::Postgres,
        &ts_table,
        &[("id".to_string(), "BIGINT".to_string())],
    );
    client.batch_execute(&ddl).await.unwrap();

    let sql = skippr::runtime_test_cdc_apply::cdc_apply::upsert_if_newer_sql(
        SqlDialect::Postgres,
        "cdc_replay",
        &ts_table,
        &[
            "\"id\"".to_string(),
            "\"val\"".to_string(),
            "\"_skippr_order_token\"".to_string(),
        ],
        &[
            "1".to_string(),
            "'first'".to_string(),
            "decode('0000000000000001', 'hex')".to_string(),
        ],
        &["\"id\"".to_string()],
        "0000000000000001",
    );

    // Execute twice — should be idempotent
    client.batch_execute(&sql).await.unwrap();
    client.batch_execute(&sql).await.unwrap();

    let count: i64 = client
        .query_one("SELECT count(*) FROM cdc_replay", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 1);

    let val: String = client
        .query_one("SELECT val FROM cdc_replay WHERE id = 1", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(val, "first");

    // Cleanup
    client
        .batch_execute("DROP TABLE IF EXISTS \"_skippr_tombstones_cdc_replay\"; DROP TABLE IF EXISTS cdc_replay;")
        .await
        .unwrap();
}
