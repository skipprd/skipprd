/// CDC Contract / Reference Path SQL Tests (secondary coverage)
///
/// Validates structural contracts and generated SQL for the exact-once CDC
/// pipeline on the Postgres sink apply path without a running database.
use std::collections::HashMap;
use std::time::SystemTime;

use skippr_core::buffer::segment_file::{PartitionKey, SegmentFile};
use skippr_core::helpers::offsets::OffsetKey;
use skippr_core::plugins::cdc::*;
use skippr_plugin_data_sink_postgres::{
    ddl_add_order_token_column, ddl_create_tombstone_table, delete_if_newer_sql,
    tombstone_table_name, upsert_if_newer_sql, PostgresCdcBackend,
};

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn temp_dir() -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!("skippr_cdc_ref_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&base);
    base
}

fn make_batch() -> arrow::record_batch::RecordBatch {
    use arrow::array::{Int64Array, StringArray};
    use arrow::datatypes::{DataType, Field, Schema};

    let schema = Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("name", DataType::Utf8, true),
    ]);
    arrow::record_batch::RecordBatch::try_new(
        std::sync::Arc::new(schema),
        vec![
            std::sync::Arc::new(Int64Array::from(vec![1, 2, 3])),
            std::sync::Arc::new(StringArray::from(vec![
                Some("Alice"),
                Some("Bob"),
                Some("Charlie"),
            ])),
        ],
    )
    .unwrap()
}

#[test]
fn reference_path_tombstone_table_naming() {
    assert_eq!(
        tombstone_table_name("\"public\".\"users\""),
        "\"public\".\"_skippr_tombstones_users\""
    );
    assert_eq!(
        tombstone_table_name("\"my_catalog\".\"my_schema\".\"orders\""),
        "\"my_catalog\".\"my_schema\".\"_skippr_tombstones_orders\""
    );
}

#[test]
fn reference_path_ddl_order_token_column() {
    let sql = ddl_add_order_token_column::<PostgresCdcBackend>("\"public\".\"users\"");
    assert_eq!(
        sql,
        "ALTER TABLE \"public\".\"users\" ADD COLUMN IF NOT EXISTS \"_skippr_order_token\" BYTEA"
    );
}

#[test]
fn reference_path_ddl_tombstone_table() {
    let sql = ddl_create_tombstone_table::<PostgresCdcBackend>(
        "\"public\".\"_skippr_tombstones_users\"",
        &[("id".to_string(), "BIGINT".to_string())],
    );
    assert!(sql.contains("\"_skippr_order_token\" BYTEA NOT NULL"));
    assert!(sql.contains("PRIMARY KEY (\"id\")"));
}

#[test]
fn reference_path_upsert_sql_has_stale_write_guard() {
    let sql = upsert_if_newer_sql::<PostgresCdcBackend>(
        "\"public\".\"users\"",
        "\"public\".\"_skippr_tombstones_users\"",
        &[
            "\"id\"".to_string(),
            "\"name\"".to_string(),
            "\"_skippr_order_token\"".to_string(),
        ],
        &[
            "1".to_string(),
            "'Alice'".to_string(),
            "decode('0000000000000001', 'hex')".to_string(),
        ],
        &["\"id\"".to_string()],
        "0000000000000001",
    );
    assert!(sql.starts_with("BEGIN;"));
    assert!(sql.ends_with("COMMIT;"));
    assert!(sql.contains("_skippr_order_token\" < decode('0000000000000001', 'hex')"));
}

#[test]
fn reference_path_delete_sql_writes_tombstone() {
    let sql = delete_if_newer_sql::<PostgresCdcBackend>(
        "\"public\".\"users\"",
        "\"public\".\"_skippr_tombstones_users\"",
        &["\"id\"".to_string()],
        &["1".to_string()],
        &["BIGINT".to_string()],
        "0000000000000002",
    );
    assert!(sql.starts_with("BEGIN;"));
    assert!(sql.contains("ON CONFLICT"));
    assert!(sql.contains("_skippr_order_token\" < EXCLUDED.\"_skippr_order_token\""));
}

#[test]
fn reference_path_full_lifecycle_insert_update_delete() {
    let dir = temp_dir();
    let seg = SegmentFile::new(&dir, "lifecycle").unwrap();

    let key = PartitionKey {
        sink_ref: "data_outputs.pg".to_string(),
        namespace: "users".to_string(),
        partition: "".to_string(),
        time: None,
        shard: "".to_string(),
    };

    let batch = make_batch();
    let mut batches = HashMap::new();
    batches.insert(key.clone(), vec![batch]);
    let mut parts_meta = HashMap::new();
    parts_meta.insert(key.clone(), (0u64, SystemTime::now()));
    let offsets: HashMap<OffsetKey, u64> = HashMap::new();
    let meta = WalPartMeta {
        kind: WalPartKind::Cdc,
        row_count: 3,
        rows: vec![
            WalRowMeta {
                mutation: MutationKind::Insert,
                event_id: b"lsn:100".to_vec(),
                order_token: vec![0, 0, 0, 100],
            },
            WalRowMeta {
                mutation: MutationKind::Update,
                event_id: b"lsn:101".to_vec(),
                order_token: vec![0, 0, 0, 101],
            },
            WalRowMeta {
                mutation: MutationKind::Delete,
                event_id: b"lsn:102".to_vec(),
                order_token: vec![0, 0, 0, 102],
            },
        ],
    };

    let mut blobs = HashMap::new();
    blobs.insert(key.clone(), bincode::serialize(&meta).unwrap());
    seg.write_snapshot(&offsets, &batches, &parts_meta, &blobs)
        .unwrap();

    let mut f = std::fs::File::open(&seg.path).unwrap();
    let read_blobs = SegmentFile::read_part_meta_blobs_from_reader(&mut f).unwrap();
    let decoded: WalPartMeta = bincode::deserialize(read_blobs.get(&key).unwrap()).unwrap();
    let actions: Vec<SinkApplyAction> = decoded
        .rows
        .iter()
        .enumerate()
        .map(|(i, row_meta)| {
            resolve_apply_action(
                row_meta.mutation,
                vec![("id".to_string(), format!("{}", i + 1))],
                row_meta.order_token.clone(),
            )
        })
        .collect();

    let fq_table = "\"public\".\"users\"";
    let fq_tombstone = tombstone_table_name(fq_table);
    for action in &actions {
        match action {
            SinkApplyAction::UpsertIfNewer {
                business_key,
                order_token,
            } => {
                let hex = to_hex(order_token);
                let sql = upsert_if_newer_sql::<PostgresCdcBackend>(
                    fq_table,
                    &fq_tombstone,
                    &[
                        "\"id\"".to_string(),
                        "\"name\"".to_string(),
                        "\"_skippr_order_token\"".to_string(),
                    ],
                    &[
                        business_key[0].1.clone(),
                        "'test'".to_string(),
                        format!("decode('{}', 'hex')", hex),
                    ],
                    &["\"id\"".to_string()],
                    &hex,
                );
                assert!(sql.contains("BEGIN;"));
            }
            SinkApplyAction::DeleteIfNewer {
                business_key,
                order_token,
            } => {
                let hex = to_hex(order_token);
                let sql = delete_if_newer_sql::<PostgresCdcBackend>(
                    fq_table,
                    &fq_tombstone,
                    &["\"id\"".to_string()],
                    &[business_key[0].1.clone()],
                    &["BIGINT".to_string()],
                    &hex,
                );
                assert!(sql.contains("ON CONFLICT"));
            }
        }
    }
}

#[test]
fn reference_path_replay_and_stale_write_invariants() {
    let token = "00000064";
    let fq_table = "\"public\".\"users\"";
    let fq_tombstone = "\"public\".\"_skippr_tombstones_users\"";

    let sql1 = upsert_if_newer_sql::<PostgresCdcBackend>(
        fq_table,
        fq_tombstone,
        &[
            "\"id\"".to_string(),
            "\"name\"".to_string(),
            "\"_skippr_order_token\"".to_string(),
        ],
        &[
            "1".to_string(),
            "'Alice'".to_string(),
            format!("decode('{}', 'hex')", token),
        ],
        &["\"id\"".to_string()],
        token,
    );
    let sql2 = upsert_if_newer_sql::<PostgresCdcBackend>(
        fq_table,
        fq_tombstone,
        &[
            "\"id\"".to_string(),
            "\"name\"".to_string(),
            "\"_skippr_order_token\"".to_string(),
        ],
        &[
            "1".to_string(),
            "'Alice'".to_string(),
            format!("decode('{}', 'hex')", token),
        ],
        &["\"id\"".to_string()],
        token,
    );
    assert_eq!(sql1, sql2);
    assert!(sql1.contains("_skippr_order_token\" < decode('00000064', 'hex')"));

    let delete_sql = delete_if_newer_sql::<PostgresCdcBackend>(
        fq_table,
        fq_tombstone,
        &["\"id\"".to_string()],
        &["1".to_string()],
        &["BIGINT".to_string()],
        "00000064",
    );
    assert!(delete_sql.contains("_skippr_order_token\" < EXCLUDED.\"_skippr_order_token\""));
}
