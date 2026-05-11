/// CDC Protocol Tests: Postgres (secondary coverage)
///
/// Low-level tests that validate Postgres CDC protocol details, WAL
/// metadata roundtrips, and generated SQL at the driver/SQL level.
/// These are NOT the CDC acceptance gate — see `cdc_e2e_postgres` for
/// the full-pipeline end-to-end suite.
///
/// Requires Docker services `postgres` and `postgres-target`.
use std::collections::HashMap;
use std::time::SystemTime;

use skippr_plugin_data_sink_postgres::{
    ddl_add_order_token_column, ddl_create_tombstone_table, delete_if_newer_sql,
    tombstone_table_name, upsert_if_newer_sql, PostgresCdcBackend,
};
use skippr_runtime_sdk::plugins::cdc::{MutationKind, WalPartKind, WalPartMeta, WalRowMeta};
use skippr_runtime_sdk::sink_compat::segment_file::{PartitionKey, SegmentFile};

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
}

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
    assert!(!lsn.is_empty());
}

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

    let ddl1 = ddl_add_order_token_column::<PostgresCdcBackend>("cdc_target");
    client.batch_execute(&ddl1).await.unwrap();

    let ts_table = tombstone_table_name("cdc_target");
    let ddl2 = ddl_create_tombstone_table::<PostgresCdcBackend>(
        &ts_table,
        &[("id".to_string(), "BIGINT".to_string())],
    );
    client.batch_execute(&ddl2).await.unwrap();
}

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
    let ddl = ddl_create_tombstone_table::<PostgresCdcBackend>(
        &ts_table,
        &[("id".to_string(), "BIGINT".to_string())],
    );
    client.batch_execute(&ddl).await.unwrap();

    let sql1 = upsert_if_newer_sql::<PostgresCdcBackend>(
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

    let sql2 = upsert_if_newer_sql::<PostgresCdcBackend>(
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
    assert_eq!(name, "Alice");
}

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
    let ddl = ddl_create_tombstone_table::<PostgresCdcBackend>(
        &ts_table,
        &[("id".to_string(), "BIGINT".to_string())],
    );
    client.batch_execute(&ddl).await.unwrap();

    let sql_insert = upsert_if_newer_sql::<PostgresCdcBackend>(
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

    let sql_delete = delete_if_newer_sql::<PostgresCdcBackend>(
        "cdc_del",
        &ts_table,
        &["\"id\"".to_string()],
        &["1".to_string()],
        &["BIGINT".to_string()],
        "0000000000000003",
    );
    client.batch_execute(&sql_delete).await.unwrap();

    let sql_stale_insert = upsert_if_newer_sql::<PostgresCdcBackend>(
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
    assert_eq!(count, 0);
}
