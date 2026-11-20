/// CDC Contract / Reference Path SQL Tests (secondary coverage)
///
/// Validates structural contracts and generated SQL for the exact-once CDC
/// pipeline on the Postgres sink apply path without a running database.
use std::collections::HashMap;
use std::time::SystemTime;

use skippr_plugin_data_sink_postgres::{
    ddl_add_order_token_column, ddl_create_tombstone_table, delete_if_newer_sql,
    postgres_bulk_cdc_sql, tombstone_table_name, upsert_if_newer_sql, CdcApplyBatch,
    CdcApplyColumn, CdcApplyMutation, CdcApplyRow, CdcApplyRowMetadata, CdcApplyValue,
    PostgresCdcBackend,
};
use skippr_runtime_sdk::plugins::cdc::*;
use skippr_runtime_sdk::progress::OffsetKey;
use skippr_runtime_sdk::sink_compat::segment_file::{PartitionKey, SegmentFile, WalPayload};

fn incoming(order: Vec<u8>) -> skippr_cdc::StoredWinner {
    skippr_cdc::StoredWinner {
        order,
        tie: skippr_cdc::Tie {
            seq: 1,
            ordinal: 0,
            row: 0,
        },
    }
}

fn incoming_hex(hex: &str) -> skippr_cdc::StoredWinner {
    incoming(hex::decode(hex).expect("hex"))
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

fn bulk_apply_batch(rows: Vec<CdcApplyRow>) -> CdcApplyBatch {
    CdcApplyBatch {
        columns: vec![
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
        ],
        business_key_columns: vec!["tenant_id".to_string(), "id".to_string()],
        rows,
    }
}

fn bulk_apply_row(
    mutation: CdcApplyMutation,
    ordinal: u64,
    token: u8,
    tenant_id: i64,
    id: i64,
    name: &str,
) -> CdcApplyRow {
    CdcApplyRow {
        metadata: CdcApplyRowMetadata {
            mutation,
            event_id: format!("event-{ordinal}").into_bytes(),
            order_token: vec![token],
            tie: skippr_cdc::Tie {
                seq: 1,
                ordinal: 0,
                row: ordinal,
            },
        },
        values: vec![
            CdcApplyValue::Signed(tenant_id),
            CdcApplyValue::Signed(id),
            CdcApplyValue::Text(name.to_string()),
        ],
    }
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
        "ALTER TABLE \"public\".\"users\" ADD COLUMN IF NOT EXISTS \"_skippr_order_token\" BYTEA;\n\
         ALTER TABLE \"public\".\"users\" ADD COLUMN IF NOT EXISTS \"_skippr_tie_seq\" BIGINT;\n\
         ALTER TABLE \"public\".\"users\" ADD COLUMN IF NOT EXISTS \"_skippr_tie_ordinal\" INTEGER;\n\
         ALTER TABLE \"public\".\"users\" ADD COLUMN IF NOT EXISTS \"_skippr_tie_row\" BIGINT"
    );
}

#[test]
fn reference_path_ddl_tombstone_table() {
    let sql = ddl_create_tombstone_table::<PostgresCdcBackend>(
        "\"public\".\"_skippr_tombstones_users\"",
        &[("id".to_string(), "BIGINT")],
    );
    assert!(sql.contains("\"_skippr_order_token\" BYTEA NOT NULL"));
    assert!(sql.contains("\"_skippr_tie_seq\" BIGINT NOT NULL"));
    assert!(sql.contains("PRIMARY KEY (\"id\")"));
}

#[test]
fn reference_path_upsert_sql_has_stale_write_guard() {
    let incoming = skippr_cdc::StoredWinner {
        order: vec![0, 0, 0, 0, 0, 0, 0, 1],
        tie: skippr_cdc::Tie {
            seq: 1,
            ordinal: 0,
            row: 0,
        },
    };
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
        &incoming,
    );
    assert!(sql.starts_with("BEGIN;"));
    assert!(sql.ends_with("COMMIT;"));
    assert!(sql.contains("_skippr_tie_seq"));
}

#[test]
fn reference_path_delete_sql_writes_tombstone() {
    let incoming = skippr_cdc::StoredWinner {
        order: vec![0, 0, 0, 0, 0, 0, 0, 2],
        tie: skippr_cdc::Tie {
            seq: 1,
            ordinal: 0,
            row: 1,
        },
    };
    let sql = delete_if_newer_sql::<PostgresCdcBackend>(
        "\"public\".\"users\"",
        "\"public\".\"_skippr_tombstones_users\"",
        &["\"id\"".to_string()],
        &["1".to_string()],
        &["BIGINT".to_string()],
        &incoming,
    );
    assert!(sql.starts_with("BEGIN;"));
    assert!(sql.contains("ON CONFLICT"));
    assert!(sql.contains("_skippr_tie_seq"));
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
        schema_fingerprint: "".to_string(),
    };

    let batch = make_batch();
    let mut batches = HashMap::new();
    batches.insert(key.clone(), WalPayload::Arrow(vec![batch]));
    let mut parts_meta = HashMap::new();
    parts_meta.insert(key.clone(), (0u64, SystemTime::now()));
    let offsets: HashMap<OffsetKey, u64> = HashMap::new();
    let meta = WalPartMeta {
        kind: WalPartKind::Cdc,
        row_count: 3,
        rows: vec![
            WalRowMeta::new(
                MutationKind::Insert,
                b"lsn:100".to_vec(),
                vec![0, 0, 0, 100],
            ),
            WalRowMeta::new(
                MutationKind::Update,
                b"lsn:101".to_vec(),
                vec![0, 0, 0, 101],
            ),
            WalRowMeta::new(
                MutationKind::Delete,
                b"lsn:102".to_vec(),
                vec![0, 0, 0, 102],
            ),
        ],
    };

    let mut blobs = HashMap::new();
    blobs.insert(key.clone(), bincode::serialize(&meta).unwrap());
    seg.write_snapshot(
        &offsets,
        &batches,
        &parts_meta,
        &blobs,
        std::time::SystemTime::UNIX_EPOCH,
    )
    .unwrap();

    let mut f = std::fs::File::open(&seg.path).unwrap();
    let read_blobs = SegmentFile::read_part_meta_blobs_from_reader(&mut f).unwrap();
    let decoded: WalPartMeta = bincode::deserialize(read_blobs.get(&key).unwrap()).unwrap();
    let fq_table = "\"public\".\"users\"";
    let fq_tombstone = tombstone_table_name(fq_table);
    for (i, row_meta) in decoded.rows.iter().enumerate() {
        let winner = incoming(row_meta.order_token.clone());
        let hex = hex::encode(&row_meta.order_token);
        match row_meta.mutation {
            MutationKind::Insert | MutationKind::Update | MutationKind::Snapshot => {
                let sql = upsert_if_newer_sql::<PostgresCdcBackend>(
                    fq_table,
                    &fq_tombstone,
                    &[
                        "\"id\"".to_string(),
                        "\"name\"".to_string(),
                        "\"_skippr_order_token\"".to_string(),
                    ],
                    &[
                        format!("{}", i + 1),
                        "'test'".to_string(),
                        format!("decode('{hex}', 'hex')"),
                    ],
                    &["\"id\"".to_string()],
                    &winner,
                );
                assert!(sql.contains("BEGIN;"));
            }
            MutationKind::Delete => {
                let sql = delete_if_newer_sql::<PostgresCdcBackend>(
                    fq_table,
                    &fq_tombstone,
                    &["\"id\"".to_string()],
                    &[format!("{}", i + 1)],
                    &["BIGINT".to_string()],
                    &winner,
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

    let winner = incoming_hex(token);
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
        &winner,
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
        &winner,
    );
    assert_eq!(sql1, sql2);
    assert!(sql1.contains("_skippr_tie_seq"));

    let delete_sql = delete_if_newer_sql::<PostgresCdcBackend>(
        fq_table,
        fq_tombstone,
        &["\"id\"".to_string()],
        &["1".to_string()],
        &["BIGINT".to_string()],
        &winner,
    );
    assert!(delete_sql.contains("_skippr_tie_seq"));
}

#[test]
fn reference_bulk_path_generates_one_set_based_apply_for_mixed_rows() {
    let batch = bulk_apply_batch(vec![
        bulk_apply_row(CdcApplyMutation::Upsert, 0, 1, 10, 1, "first"),
        bulk_apply_row(CdcApplyMutation::Upsert, 1, 1, 10, 1, "replay"),
        bulk_apply_row(CdcApplyMutation::Delete, 2, 3, 10, 1, ""),
        bulk_apply_row(CdcApplyMutation::Upsert, 3, 2, 20, 1, "other tenant"),
        bulk_apply_row(CdcApplyMutation::Upsert, 4, 4, 20, 2, "other key"),
        bulk_apply_row(CdcApplyMutation::Upsert, 5, 1, 20, 1, "stale"),
    ]);

    let sql = postgres_bulk_cdc_sql(
        "\"public\".\"users\"",
        "\"public\".\"_skippr_tombstones_users\"",
        &batch,
    )
    .unwrap();

    assert!(sql.create_stage_table.starts_with("CREATE TEMP TABLE"));
    assert!(sql.create_stage_table.ends_with("ON COMMIT DROP;"));
    assert!(sql.copy_into_stage.starts_with("COPY pg_temp."));
    assert!(sql.copy_into_stage.ends_with("WITH (FORMAT text)"));
    assert!(!sql.materialize_winners.contains("ROW_NUMBER()"));
    assert!(sql.materialize_winners.contains("SELECT * FROM pg_temp."));
    assert!(sql.apply_winners.contains("_skippr_cdc_tie_seq"));
    assert_eq!(
        sql.apply_winners
            .matches("INSERT INTO \"public\".\"users\" AS target")
            .count(),
        1
    );
    assert!(sql
        .apply_winners
        .contains("ON CONFLICT (\"tenant_id\", \"id\") DO UPDATE"));
    assert!(sql
        .apply_winners
        .contains("DELETE FROM \"public\".\"users\" AS target"));
    assert!(sql
        .apply_winners
        .contains("INSERT INTO \"public\".\"_skippr_tombstones_users\" AS tombstone"));
    assert!(sql
        .apply_winners
        .contains("DELETE FROM \"public\".\"_skippr_tombstones_users\" AS tombstone"));
    assert!(!sql.apply_winners.contains("BEGIN;"));
    assert!(!sql.apply_winners.contains("COMMIT;"));
}

#[test]
fn reference_bulk_path_rejects_missing_business_key_column() {
    let mut batch = bulk_apply_batch(vec![bulk_apply_row(
        CdcApplyMutation::Upsert,
        0,
        1,
        10,
        1,
        "first",
    )]);
    batch.business_key_columns.push("missing".to_string());

    let error = postgres_bulk_cdc_sql(
        "\"public\".\"users\"",
        "\"public\".\"_skippr_tombstones_users\"",
        &batch,
    )
    .unwrap_err();

    assert!(error
        .to_string()
        .contains("\"missing\" is not in the batch"));
}
