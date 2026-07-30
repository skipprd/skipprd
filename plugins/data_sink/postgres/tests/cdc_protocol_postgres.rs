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
    apply_postgres_cdc_batch, ddl_add_order_token_column, ddl_create_tombstone_table,
    delete_if_newer_sql, tombstone_table_name, upsert_if_newer_sql, CdcApplyBatch, CdcApplyColumn,
    CdcApplyMutation, CdcApplyRow, CdcApplyRowMetadata, CdcApplyValue, PostgresCdcBackend,
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

fn bulk_row(
    mutation: CdcApplyMutation,
    ordinal: u64,
    token: u8,
    values: Vec<CdcApplyValue>,
) -> CdcApplyRow {
    CdcApplyRow {
        metadata: CdcApplyRowMetadata {
            mutation,
            event_id: format!("event-{ordinal}").into_bytes(),
            order_token: vec![token],
            source_ordinal: ordinal,
        },
        values,
    }
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
        schema_fingerprint: "".to_string(),
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

#[tokio::test]
#[ignore]
async fn cdc_bulk_stage_preserves_exact_final_state_contract() {
    let mut client = connect(TARGET_CONN).await;
    client
        .batch_execute(
            "DROP TABLE IF EXISTS \"_skippr_tombstones_cdc_bulk_contract\"; \
             DROP TABLE IF EXISTS cdc_bulk_contract; \
             CREATE TABLE cdc_bulk_contract (\
               tenant_id BIGINT NOT NULL, \
               id BIGINT NOT NULL, \
               name TEXT, \
               \"_skippr_order_token\" BYTEA, \
               PRIMARY KEY (tenant_id, id)\
             );",
        )
        .await
        .unwrap();

    let tombstone = tombstone_table_name("cdc_bulk_contract");
    let tombstone_ddl = ddl_create_tombstone_table::<PostgresCdcBackend>(
        &tombstone,
        &[
            ("tenant_id".to_string(), "BIGINT".to_string()),
            ("id".to_string(), "BIGINT".to_string()),
        ],
    );
    client.batch_execute(&tombstone_ddl).await.unwrap();

    let columns = vec![
        CdcApplyColumn {
            name: "tenant_id".to_string(),
            target_type: "BIGINT".to_string(),
        },
        CdcApplyColumn {
            name: "id".to_string(),
            target_type: "BIGINT".to_string(),
        },
        CdcApplyColumn {
            name: "name".to_string(),
            target_type: "TEXT".to_string(),
        },
    ];
    let initial = CdcApplyBatch {
        columns: columns.clone(),
        business_key_columns: vec!["tenant_id".to_string(), "id".to_string()],
        rows: vec![
            bulk_row(
                CdcApplyMutation::Upsert,
                0,
                1,
                vec![
                    CdcApplyValue::Signed(10),
                    CdcApplyValue::Signed(1),
                    CdcApplyValue::Text("first".to_string()),
                ],
            ),
            bulk_row(
                CdcApplyMutation::Upsert,
                1,
                2,
                vec![
                    CdcApplyValue::Signed(20),
                    CdcApplyValue::Signed(1),
                    CdcApplyValue::Text("second".to_string()),
                ],
            ),
            bulk_row(
                CdcApplyMutation::Delete,
                2,
                3,
                vec![
                    CdcApplyValue::Signed(10),
                    CdcApplyValue::Signed(1),
                    CdcApplyValue::Null,
                ],
            ),
            bulk_row(
                CdcApplyMutation::Upsert,
                3,
                4,
                vec![
                    CdcApplyValue::Signed(20),
                    CdcApplyValue::Signed(2),
                    CdcApplyValue::Text("third".to_string()),
                ],
            ),
            bulk_row(
                CdcApplyMutation::Upsert,
                4,
                1,
                vec![
                    CdcApplyValue::Signed(20),
                    CdcApplyValue::Signed(1),
                    CdcApplyValue::Text("stale".to_string()),
                ],
            ),
            bulk_row(
                CdcApplyMutation::Upsert,
                5,
                4,
                vec![
                    CdcApplyValue::Signed(20),
                    CdcApplyValue::Signed(2),
                    CdcApplyValue::Text("equal-token replay".to_string()),
                ],
            ),
        ],
    };

    assert_eq!(
        apply_postgres_cdc_batch(&mut client, "cdc_bulk_contract", &tombstone, &initial)
            .await
            .unwrap(),
        initial.rows.len()
    );
    apply_postgres_cdc_batch(&mut client, "cdc_bulk_contract", &tombstone, &initial)
        .await
        .unwrap();

    let live_rows = client
        .query(
            "SELECT tenant_id, id, name, encode(\"_skippr_order_token\", 'hex') \
             FROM cdc_bulk_contract ORDER BY tenant_id, id",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(live_rows.len(), 2);
    assert_eq!(live_rows[0].get::<_, i64>(0), 20);
    assert_eq!(live_rows[0].get::<_, i64>(1), 1);
    assert_eq!(live_rows[0].get::<_, String>(2), "second");
    assert_eq!(live_rows[0].get::<_, String>(3), "02");
    assert_eq!(live_rows[1].get::<_, i64>(1), 2);
    assert_eq!(live_rows[1].get::<_, String>(2), "third");
    assert_eq!(live_rows[1].get::<_, String>(3), "04");

    let tombstone_token: String = client
        .query_one(
            "SELECT encode(\"_skippr_order_token\", 'hex') \
             FROM \"_skippr_tombstones_cdc_bulk_contract\" \
             WHERE tenant_id = 10 AND id = 1",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(tombstone_token, "03");

    let late = CdcApplyBatch {
        columns: columns.clone(),
        business_key_columns: vec!["tenant_id".to_string(), "id".to_string()],
        rows: vec![
            bulk_row(
                CdcApplyMutation::Upsert,
                0,
                2,
                vec![
                    CdcApplyValue::Signed(10),
                    CdcApplyValue::Signed(1),
                    CdcApplyValue::Text("zombie".to_string()),
                ],
            ),
            bulk_row(
                CdcApplyMutation::Delete,
                1,
                1,
                vec![
                    CdcApplyValue::Signed(20),
                    CdcApplyValue::Signed(1),
                    CdcApplyValue::Null,
                ],
            ),
        ],
    };
    apply_postgres_cdc_batch(&mut client, "cdc_bulk_contract", &tombstone, &late)
        .await
        .unwrap();

    let stale_upsert_count: i64 = client
        .query_one(
            "SELECT count(*) FROM cdc_bulk_contract \
             WHERE tenant_id = 10 AND id = 1",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(stale_upsert_count, 0);
    let newer_live_name: String = client
        .query_one(
            "SELECT name FROM cdc_bulk_contract WHERE tenant_id = 20 AND id = 1",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(newer_live_name, "second");
    let stale_delete_tombstone: String = client
        .query_one(
            "SELECT encode(\"_skippr_order_token\", 'hex') \
             FROM \"_skippr_tombstones_cdc_bulk_contract\" \
             WHERE tenant_id = 20 AND id = 1",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(stale_delete_tombstone, "01");

    let resurrect = CdcApplyBatch {
        columns,
        business_key_columns: vec!["tenant_id".to_string(), "id".to_string()],
        rows: vec![bulk_row(
            CdcApplyMutation::Upsert,
            0,
            4,
            vec![
                CdcApplyValue::Signed(10),
                CdcApplyValue::Signed(1),
                CdcApplyValue::Text("resurrected".to_string()),
            ],
        )],
    };
    apply_postgres_cdc_batch(&mut client, "cdc_bulk_contract", &tombstone, &resurrect)
        .await
        .unwrap();

    let resurrected_name: String = client
        .query_one(
            "SELECT name FROM cdc_bulk_contract WHERE tenant_id = 10 AND id = 1",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(resurrected_name, "resurrected");
    let cleared_tombstone_count: i64 = client
        .query_one(
            "SELECT count(*) FROM \"_skippr_tombstones_cdc_bulk_contract\" \
             WHERE tenant_id = 10 AND id = 1",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(cleared_tombstone_count, 0);
}

#[tokio::test]
#[ignore]
async fn cdc_bulk_stage_rolls_back_bad_row_and_cleans_staging() {
    let mut client = connect(TARGET_CONN).await;
    client
        .batch_execute(
            "DROP TABLE IF EXISTS \"_skippr_tombstones_cdc_bulk_rollback\"; \
             DROP TABLE IF EXISTS cdc_bulk_rollback; \
             CREATE TABLE cdc_bulk_rollback (\
               id BIGINT PRIMARY KEY, \
               name TEXT, \
               \"_skippr_order_token\" BYTEA\
             );",
        )
        .await
        .unwrap();

    let tombstone = tombstone_table_name("cdc_bulk_rollback");
    let tombstone_ddl = ddl_create_tombstone_table::<PostgresCdcBackend>(
        &tombstone,
        &[("id".to_string(), "BIGINT".to_string())],
    );
    client.batch_execute(&tombstone_ddl).await.unwrap();

    let columns = vec![
        CdcApplyColumn {
            name: "id".to_string(),
            target_type: "BIGINT".to_string(),
        },
        CdcApplyColumn {
            name: "name".to_string(),
            target_type: "TEXT".to_string(),
        },
    ];
    let bad_batch = CdcApplyBatch {
        columns: columns.clone(),
        business_key_columns: vec!["id".to_string()],
        rows: vec![
            bulk_row(
                CdcApplyMutation::Upsert,
                0,
                1,
                vec![
                    CdcApplyValue::Signed(1),
                    CdcApplyValue::Text("valid".to_string()),
                ],
            ),
            bulk_row(
                CdcApplyMutation::Upsert,
                1,
                2,
                vec![
                    CdcApplyValue::Text("not-an-integer".to_string()),
                    CdcApplyValue::Text("invalid".to_string()),
                ],
            ),
        ],
    };

    let error = apply_postgres_cdc_batch(&mut client, "cdc_bulk_rollback", &tombstone, &bad_batch)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("CDC staging COPY"));

    let target_count: i64 = client
        .query_one("SELECT count(*) FROM cdc_bulk_rollback", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(target_count, 0);
    let stage_exists: bool = client
        .query_one(
            "SELECT to_regclass('pg_temp._skippr_cdc_stage') IS NOT NULL",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(!stage_exists);

    let retry = CdcApplyBatch {
        columns,
        business_key_columns: vec!["id".to_string()],
        rows: vec![bulk_row(
            CdcApplyMutation::Upsert,
            0,
            3,
            vec![
                CdcApplyValue::Signed(1),
                CdcApplyValue::Text("retry".to_string()),
            ],
        )],
    };
    apply_postgres_cdc_batch(&mut client, "cdc_bulk_rollback", &tombstone, &retry)
        .await
        .unwrap();

    let retry_name: String = client
        .query_one("SELECT name FROM cdc_bulk_rollback WHERE id = 1", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(retry_name, "retry");
}
