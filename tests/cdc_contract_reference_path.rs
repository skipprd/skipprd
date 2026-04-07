/// CDC Contract / Reference Path Tests  (secondary coverage)
///
/// Validates structural contracts and generated SQL for the exact-once CDC
/// pipeline on the Postgres -> Postgres reference path WITHOUT a running
/// database.  These are NOT the CDC acceptance gate — see the `cdc_e2e_*`
/// test suites for full-pipeline end-to-end coverage.
///
/// Covers:
///   WAL write/read roundtrip, compatibility validation, sink apply SQL
///   generation, stale-write rejection invariants, order-token correctness.
use std::collections::HashMap;
use std::time::SystemTime;

use skippr::buffer::segment_file::{PartitionKey, SegmentFile};
use skippr::helpers::offsets::OffsetKey;
use skippr::plugins::cdc::*;
use skippr::plugins::data_sink::cdc_apply::*;

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn temp_dir() -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!("skippr_cdc_ref_{}", rand::random::<u64>()));
    let _ = std::fs::create_dir_all(&base);
    base
}

fn make_batch() -> arrow::record_batch::RecordBatch {
    use arrow::array::Int64Array;
    use arrow::array::StringArray;
    use arrow::datatypes::{DataType, Field, Schema};

    let schema = Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("name", DataType::Utf8, true),
    ]);
    let ids = Int64Array::from(vec![1, 2, 3]);
    let names = StringArray::from(vec![Some("Alice"), Some("Bob"), Some("Charlie")]);
    arrow::record_batch::RecordBatch::try_new(
        std::sync::Arc::new(schema),
        vec![std::sync::Arc::new(ids), std::sync::Arc::new(names)],
    )
    .unwrap()
}

// -----------------------------------------------------------------------
// 1. Compatibility validation
// -----------------------------------------------------------------------

#[test]
fn reference_path_postgres_to_postgres_derives_exact_once() {
    let source = source_capabilities::POSTGRES;
    let sink = sink_capabilities::POSTGRES;
    let keys = vec!["id".to_string()];
    match derive_and_validate(&source, &sink, "users", &keys) {
        CompatibilityResult::Compatible(g) => {
            assert_eq!(g, EffectiveGuarantee::ExactOnceFinalState);
        }
        CompatibilityResult::Incompatible(reasons) => {
            panic!("postgres->postgres should be compatible: {:?}", reasons);
        }
    }
}

#[test]
fn reference_path_postgres_source_declares_correct_capabilities() {
    let cap = source_capabilities::POSTGRES;
    assert_eq!(
        cap.guarantee_tier,
        SourceGuaranteeTier::SnapshotThenLogExactOnce
    );
    assert_eq!(cap.checkpoint_style, SourceCheckpointStyle::LogNative);
    assert_eq!(cap.bootstrap_style, SourceBootstrapStyle::AnchoredSnapshot);
    assert_eq!(cap.order_model, SourceOrderModel::GlobalTotalOrder);
    assert!(cap.supports_deletes);
    assert_eq!(cap.event_id_semantics, EventIdSemantics::LogPosition);
}

#[test]
fn reference_path_postgres_sink_declares_correct_capabilities() {
    let cap = sink_capabilities::POSTGRES;
    assert_eq!(cap.guarantee_tier, SinkGuaranteeTier::ExactOnceCdcEligible);
    assert!(cap.can_manage_skippr_columns);
    assert!(cap.can_maintain_tombstone_tables);
    assert!(cap.can_compare_order_tokens);
    assert!(cap.supports_transactions);
}

// -----------------------------------------------------------------------
// 2. WAL segment write + read roundtrip with CDC metadata
// -----------------------------------------------------------------------

#[test]
fn reference_path_wal_cdc_metadata_roundtrip() {
    let dir = temp_dir();
    let seg = SegmentFile::new(&dir, "ref_cdc").unwrap();

    let key = PartitionKey {
        sink_ref: "data_outputs.pg".to_string(),
        namespace: "users".to_string(),
        partition: "".to_string(),
        time: None,
        shard: "".to_string(),
    };

    let batch = make_batch();
    let mut batches: HashMap<PartitionKey, Vec<arrow::record_batch::RecordBatch>> = HashMap::new();
    batches.insert(key.clone(), vec![batch]);
    let mut parts_meta: HashMap<PartitionKey, (u64, SystemTime)> = HashMap::new();
    parts_meta.insert(key.clone(), (0, SystemTime::now()));
    let offsets: HashMap<OffsetKey, u64> = HashMap::new();

    let wal_meta = WalPartMeta {
        kind: WalPartKind::Cdc,
        row_count: 3,
        rows: vec![
            WalRowMeta {
                mutation: MutationKind::Insert,
                event_id: b"pg:lsn:0/16B3748".to_vec(),
                order_token: vec![0, 0, 0, 0, 0x16, 0xB3, 0x74, 0x80],
            },
            WalRowMeta {
                mutation: MutationKind::Insert,
                event_id: b"pg:lsn:0/16B3749".to_vec(),
                order_token: vec![0, 0, 0, 0, 0x16, 0xB3, 0x74, 0x90],
            },
            WalRowMeta {
                mutation: MutationKind::Update,
                event_id: b"pg:lsn:0/16B374A".to_vec(),
                order_token: vec![0, 0, 0, 0, 0x16, 0xB3, 0x74, 0xA0],
            },
        ],
    };
    let meta_blob = bincode::serialize(&wal_meta).unwrap();
    let mut blobs: HashMap<PartitionKey, Vec<u8>> = HashMap::new();
    blobs.insert(key.clone(), meta_blob);

    let (seg_meta, rows, _sha) = seg
        .write_snapshot(&offsets, &batches, &parts_meta, &blobs)
        .unwrap();

    assert_eq!(rows, 3);
    assert_eq!(seg_meta.num_partitions, 1);

    // Read metadata back
    let read_meta = seg.read_metadata().unwrap();
    assert_eq!(read_meta.num_partitions, 1);

    // Read CDC metadata back
    let mut f = std::fs::File::open(&seg.path).unwrap();
    let read_blobs = SegmentFile::read_part_meta_blobs_from_reader(&mut f).unwrap();
    assert_eq!(read_blobs.len(), 1);

    let decoded: WalPartMeta = bincode::deserialize(read_blobs.get(&key).unwrap()).unwrap();
    assert_eq!(decoded.kind, WalPartKind::Cdc);
    assert_eq!(decoded.row_count, 3);
    assert_eq!(decoded.rows[0].mutation, MutationKind::Insert);
    assert_eq!(decoded.rows[1].mutation, MutationKind::Insert);
    assert_eq!(decoded.rows[2].mutation, MutationKind::Update);
    assert_eq!(decoded.rows[0].event_id, b"pg:lsn:0/16B3748");
}

// -----------------------------------------------------------------------
// 3. WAL append-mode has empty metadata
// -----------------------------------------------------------------------

#[test]
fn reference_path_wal_append_mode_has_empty_meta() {
    let dir = temp_dir();
    let seg = SegmentFile::new(&dir, "ref_append").unwrap();

    let key = PartitionKey {
        sink_ref: "data_outputs.pg".to_string(),
        namespace: "logs".to_string(),
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

    let empty_blobs: HashMap<PartitionKey, Vec<u8>> = HashMap::new();
    seg.write_snapshot(&offsets, &batches, &parts_meta, &empty_blobs)
        .unwrap();

    let meta = seg.read_metadata().unwrap();
    assert_eq!(meta.num_partitions, 1);

    let mut f = std::fs::File::open(&seg.path).unwrap();
    let blobs = SegmentFile::read_part_meta_blobs_from_reader(&mut f).unwrap();
    assert!(blobs.is_empty());
}

// -----------------------------------------------------------------------
// 4. Checkpoint envelope serialization
// -----------------------------------------------------------------------

#[test]
fn reference_path_checkpoint_envelope_authority_model() {
    let wal_owned = CheckpointEnvelope {
        authority: CheckpointAuthority::WalOwnership,
        kind: CheckpointKind::SourceResume,
        payload_version: 1,
        payload_bytes: b"pg:lsn:0/16B3748".to_vec(),
    };
    let advisory = CheckpointEnvelope {
        authority: CheckpointAuthority::AdvisoryHint,
        kind: CheckpointKind::AdvisoryProgress,
        payload_version: 1,
        payload_bytes: b"s3:last-key:data/2024/file.csv".to_vec(),
    };

    assert_eq!(wal_owned.authority, CheckpointAuthority::WalOwnership);
    assert_eq!(advisory.authority, CheckpointAuthority::AdvisoryHint);

    // Both must roundtrip through bincode
    let bytes1 = bincode::serialize(&wal_owned).unwrap();
    let bytes2 = bincode::serialize(&advisory).unwrap();
    let d1: CheckpointEnvelope = bincode::deserialize(&bytes1).unwrap();
    let d2: CheckpointEnvelope = bincode::deserialize(&bytes2).unwrap();
    assert_eq!(d1.authority, CheckpointAuthority::WalOwnership);
    assert_eq!(d2.authority, CheckpointAuthority::AdvisoryHint);
    assert_eq!(d1.kind, CheckpointKind::SourceResume);
    assert_eq!(d2.kind, CheckpointKind::AdvisoryProgress);
}

#[test]
fn reference_path_bootstrap_anchor_checkpoint() {
    let anchor = CheckpointEnvelope {
        authority: CheckpointAuthority::WalOwnership,
        kind: CheckpointKind::BootstrapAnchor,
        payload_version: 1,
        payload_bytes: b"pg:snapshot:0/16B3740".to_vec(),
    };
    let progress = CheckpointEnvelope {
        authority: CheckpointAuthority::WalOwnership,
        kind: CheckpointKind::BootstrapProgress,
        payload_version: 1,
        payload_bytes: b"pg:bootstrap:chunk:42".to_vec(),
    };

    assert_eq!(anchor.kind, CheckpointKind::BootstrapAnchor);
    assert_eq!(progress.kind, CheckpointKind::BootstrapProgress);
}

// -----------------------------------------------------------------------
// 5. Sink apply SQL generation for Postgres
// -----------------------------------------------------------------------

#[test]
fn reference_path_tombstone_table_naming() {
    assert_eq!(
        tombstone_table_name("\"public\".\"users\""),
        "\"public\".\"_skippr_tombstones_users\""
    );
    assert_eq!(
        tombstone_table_name("\"myschema\".\"orders\""),
        "\"myschema\".\"_skippr_tombstones_orders\""
    );
    // 3-part identifier (e.g. Snowflake/Databricks: catalog.schema.table)
    assert_eq!(
        tombstone_table_name("\"my_catalog\".\"my_schema\".\"orders\""),
        "\"my_catalog\".\"my_schema\".\"_skippr_tombstones_orders\""
    );
}

#[test]
fn reference_path_ddl_order_token_column() {
    let sql = ddl_add_order_token_column(SqlDialect::Postgres, "\"public\".\"users\"");
    assert_eq!(
        sql,
        "ALTER TABLE \"public\".\"users\" ADD COLUMN IF NOT EXISTS \"_skippr_order_token\" BYTEA"
    );
}

#[test]
fn reference_path_ddl_tombstone_table() {
    let sql = ddl_create_tombstone_table(
        SqlDialect::Postgres,
        "\"public\".\"_skippr_tombstones_users\"",
        &[("id".to_string(), "BIGINT".to_string())],
    );
    assert!(sql.starts_with("CREATE TABLE IF NOT EXISTS"));
    assert!(sql.contains("\"id\" BIGINT NOT NULL"));
    assert!(sql.contains("\"_skippr_order_token\" BYTEA NOT NULL"));
    assert!(sql.contains("PRIMARY KEY (\"id\")"));
}

// -----------------------------------------------------------------------
// 6. Upsert-if-newer preserves newer rows
// -----------------------------------------------------------------------

#[test]
fn reference_path_upsert_sql_has_stale_write_guard() {
    let sql = upsert_if_newer_sql(
        SqlDialect::Postgres,
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

    // Must be transactional
    assert!(sql.starts_with("BEGIN;"));
    assert!(sql.ends_with("COMMIT;"));

    // Must guard against stale writes
    assert!(sql.contains("_skippr_order_token\" IS NULL"));
    assert!(sql.contains("_skippr_order_token\" < decode('0000000000000001', 'hex')"));

    // Must check tombstone table
    assert!(sql.contains("NOT EXISTS"));
    assert!(sql.contains("_skippr_tombstones_users"));

    // Must clean up stale tombstones
    assert!(sql.contains("DELETE FROM \"public\".\"_skippr_tombstones_users\""));
}

// -----------------------------------------------------------------------
// 7. Delete-if-newer writes tombstone
// -----------------------------------------------------------------------

#[test]
fn reference_path_delete_sql_writes_tombstone() {
    let sql = delete_if_newer_sql(
        SqlDialect::Postgres,
        "\"public\".\"users\"",
        "\"public\".\"_skippr_tombstones_users\"",
        &["\"id\"".to_string()],
        &["1".to_string()],
        &["BIGINT".to_string()],
        "0000000000000002",
    );

    // Must be transactional
    assert!(sql.starts_with("BEGIN;"));
    assert!(sql.ends_with("COMMIT;"));

    // Must delete from live table with token guard
    assert!(sql.contains("DELETE FROM \"public\".\"users\""));
    assert!(sql.contains("_skippr_order_token\" < decode('0000000000000002', 'hex')"));

    // Must upsert tombstone
    assert!(sql.contains("INSERT INTO \"public\".\"_skippr_tombstones_users\""));
    assert!(sql.contains("ON CONFLICT"));

    // Tombstone upsert must only advance forward
    assert!(sql.contains("_skippr_order_token\" < EXCLUDED.\"_skippr_order_token\""));
}

// -----------------------------------------------------------------------
// 8. resolve_apply_action maps mutations correctly
// -----------------------------------------------------------------------

#[test]
fn reference_path_mutation_to_action_mapping() {
    let bk = vec![("id".to_string(), "1".to_string())];
    let token = vec![0, 0, 0, 1];

    let snapshot = resolve_apply_action(MutationKind::Snapshot, bk.clone(), token.clone());
    assert!(matches!(snapshot, SinkApplyAction::UpsertIfNewer { .. }));

    let insert = resolve_apply_action(MutationKind::Insert, bk.clone(), token.clone());
    assert!(matches!(insert, SinkApplyAction::UpsertIfNewer { .. }));

    let update = resolve_apply_action(MutationKind::Update, bk.clone(), token.clone());
    assert!(matches!(update, SinkApplyAction::UpsertIfNewer { .. }));

    let delete = resolve_apply_action(MutationKind::Delete, bk, token);
    assert!(matches!(delete, SinkApplyAction::DeleteIfNewer { .. }));
}

// -----------------------------------------------------------------------
// 9. Order token comparison correctness
// -----------------------------------------------------------------------

#[test]
fn reference_path_order_token_lexicographic_comparison() {
    // Postgres BYTEA comparison is lexicographic, which matches our token design.
    // Verify that our token encoding maintains the invariant.
    let older: Vec<u8> = vec![0, 0, 0, 0, 0x16, 0xB3, 0x74, 0x80];
    let newer: Vec<u8> = vec![0, 0, 0, 0, 0x16, 0xB3, 0x74, 0x90];
    assert!(older < newer, "older token must sort before newer token");

    // Snapshot tokens (phase 0) must sort before log tokens (phase 1)
    let snapshot_token: Vec<u8> = vec![0, 0, 0, 0, 0x16, 0xB3, 0x74, 0x80];
    let log_token: Vec<u8> = vec![1, 0, 0, 0, 0x16, 0xB3, 0x74, 0x80];
    assert!(
        snapshot_token < log_token,
        "snapshot tokens must sort before log tokens with same LSN"
    );
}

// -----------------------------------------------------------------------
// 10. Full CDC lifecycle: WAL write -> read -> apply action chain
// -----------------------------------------------------------------------

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

    // Simulate a CDC slice with insert, update, delete
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
    let meta_blob = bincode::serialize(&meta).unwrap();
    let mut blobs = HashMap::new();
    blobs.insert(key.clone(), meta_blob);

    // Step 1: Write V3 segment
    seg.write_snapshot(&offsets, &batches, &parts_meta, &blobs)
        .unwrap();

    // Step 2: Read back CDC metadata
    let mut f = std::fs::File::open(&seg.path).unwrap();
    let read_blobs = SegmentFile::read_part_meta_blobs_from_reader(&mut f).unwrap();
    let decoded: WalPartMeta = bincode::deserialize(read_blobs.get(&key).unwrap()).unwrap();

    // Step 3: Map each row to a sink apply action
    let actions: Vec<SinkApplyAction> = decoded
        .rows
        .iter()
        .enumerate()
        .map(|(i, row_meta)| {
            let bk_val = format!("{}", i + 1);
            resolve_apply_action(
                row_meta.mutation,
                vec![("id".to_string(), bk_val)],
                row_meta.order_token.clone(),
            )
        })
        .collect();

    assert!(matches!(actions[0], SinkApplyAction::UpsertIfNewer { .. }));
    assert!(matches!(actions[1], SinkApplyAction::UpsertIfNewer { .. }));
    assert!(matches!(actions[2], SinkApplyAction::DeleteIfNewer { .. }));

    // Step 4: Generate SQL for each action
    let fq_table = "\"public\".\"users\"";
    let fq_tombstone = tombstone_table_name(fq_table);

    for action in &actions {
        match action {
            SinkApplyAction::UpsertIfNewer {
                business_key,
                order_token,
            } => {
                let hex = to_hex(order_token);
                let sql = upsert_if_newer_sql(
                    SqlDialect::Postgres,
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
                assert!(sql.contains("COMMIT;"));
            }
            SinkApplyAction::DeleteIfNewer {
                business_key,
                order_token,
            } => {
                let hex = to_hex(order_token);
                let sql = delete_if_newer_sql(
                    SqlDialect::Postgres,
                    fq_table,
                    &fq_tombstone,
                    &["\"id\"".to_string()],
                    &[business_key[0].1.clone()],
                    &["BIGINT".to_string()],
                    &hex,
                );
                assert!(sql.contains("BEGIN;"));
                assert!(sql.contains("COMMIT;"));
            }
        }
    }
}

// -----------------------------------------------------------------------
// 11. Replay safety: same slice applied twice must not change outcome
// -----------------------------------------------------------------------

#[test]
fn reference_path_replay_sql_is_idempotent_by_design() {
    let token = "00000064";
    let fq_table = "\"public\".\"users\"";
    let fq_tombstone = "\"public\".\"_skippr_tombstones_users\"";

    // Two identical upserts with the same token
    let sql1 = upsert_if_newer_sql(
        SqlDialect::Postgres,
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
    let sql2 = upsert_if_newer_sql(
        SqlDialect::Postgres,
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

    // SQL is deterministic - same input produces same output
    assert_eq!(sql1, sql2);

    // The WHERE clause ensures the second execution is a no-op because
    // the existing token is NOT less than the incoming token (they're equal)
    assert!(sql1.contains("_skippr_order_token\" < decode('00000064', 'hex')"));
}

// -----------------------------------------------------------------------
// 12. Stale write rejection: older mutation cannot overwrite newer
// -----------------------------------------------------------------------

#[test]
fn reference_path_stale_write_rejection_invariant() {
    let fq_table = "\"public\".\"users\"";
    let fq_tombstone = "\"public\".\"_skippr_tombstones_users\"";

    // Newer row already applied with token 0x65
    // Older mutation arrives with token 0x64
    let older_sql = upsert_if_newer_sql(
        SqlDialect::Postgres,
        fq_table,
        fq_tombstone,
        &[
            "\"id\"".to_string(),
            "\"name\"".to_string(),
            "\"_skippr_order_token\"".to_string(),
        ],
        &[
            "1".to_string(),
            "'OldAlice'".to_string(),
            "decode('00000064', 'hex')".to_string(),
        ],
        &["\"id\"".to_string()],
        "00000064",
    );

    // The WHERE clause `_skippr_order_token < decode('00000064', 'hex')`
    // ensures this UPDATE only succeeds if the existing token (0x65) < 0x64,
    // which is FALSE, so the stale write is rejected.
    assert!(older_sql.contains("_skippr_order_token\" < decode('00000064', 'hex')"));
}

// -----------------------------------------------------------------------
// 13. Delete cannot resurrect after newer insert
// -----------------------------------------------------------------------

#[test]
fn reference_path_delete_cannot_resurrect_after_newer_insert() {
    let fq_table = "\"public\".\"users\"";
    let fq_tombstone = "\"public\".\"_skippr_tombstones_users\"";

    // Delete arrives with token 0x64, but row was already inserted with 0x65
    let delete_sql = delete_if_newer_sql(
        SqlDialect::Postgres,
        fq_table,
        fq_tombstone,
        &["\"id\"".to_string()],
        &["1".to_string()],
        &["BIGINT".to_string()],
        "00000064",
    );

    // DELETE only fires if existing token < incoming token
    assert!(delete_sql.contains("_skippr_order_token\" < decode('00000064', 'hex')"));

    // Tombstone upsert only advances if existing tombstone < incoming
    assert!(delete_sql.contains("_skippr_order_token\" < EXCLUDED.\"_skippr_order_token\""));
}

// -----------------------------------------------------------------------
// 14. Source order model correctness
// -----------------------------------------------------------------------

#[test]
fn reference_path_postgres_source_order_model_is_global_total() {
    let cap = source_capabilities::POSTGRES;
    assert_eq!(cap.order_model, SourceOrderModel::GlobalTotalOrder);
}

#[test]
fn reference_path_unsupported_source_incompatible() {
    let source = source_capabilities::STDIN;
    let sink = sink_capabilities::POSTGRES;
    let keys = vec!["id".to_string()];
    match derive_and_validate(&source, &sink, "logs", &keys) {
        CompatibilityResult::Compatible(g) => {
            assert_eq!(g, EffectiveGuarantee::CdcEncoded,
                "stdin cannot do exact-once, should fall to cdc-encoded");
        }
        CompatibilityResult::Incompatible(_) => {}
    }
}

// -----------------------------------------------------------------------
// 15. Source CDC helper: MongoDB cluster time encoding
// -----------------------------------------------------------------------

#[test]
fn source_helper_mongodb_cluster_time_order_token_encoding() {
    // MongoDB order tokens use `(seconds << 32 | increment).to_be_bytes()`
    let seconds: u32 = 1_700_000_000;
    let increment: u32 = 42;
    let combined = ((seconds as u64) << 32) | (increment as u64);
    let token = combined.to_be_bytes().to_vec();

    assert_eq!(token.len(), 8);
    // Verify big-endian layout: first 4 bytes are seconds, last 4 are increment
    let recovered = u64::from_be_bytes(token.clone().try_into().unwrap());
    assert_eq!(recovered >> 32, seconds as u64);
    assert_eq!(recovered & 0xFFFF_FFFF, increment as u64);
}

#[test]
fn source_helper_mongodb_cluster_time_lexicographic_ordering() {
    // Earlier cluster time must sort before later
    let earlier = {
        let t = ((100u64) << 32) | 50u64;
        t.to_be_bytes().to_vec()
    };
    let later = {
        let t = ((200u64) << 32) | 1u64;
        t.to_be_bytes().to_vec()
    };
    assert!(
        earlier < later,
        "earlier cluster time must sort before later"
    );

    // Same seconds, different increment
    let low_inc = {
        let t = ((100u64) << 32) | 5u64;
        t.to_be_bytes().to_vec()
    };
    let high_inc = {
        let t = ((100u64) << 32) | 500u64;
        t.to_be_bytes().to_vec()
    };
    assert!(
        low_inc < high_inc,
        "same seconds: lower increment must sort first"
    );
}

// -----------------------------------------------------------------------
// 16. Mutation fidelity preservation
// -----------------------------------------------------------------------

#[test]
fn reference_path_wal_preserves_all_four_mutation_kinds() {
    let kinds = [
        MutationKind::Snapshot,
        MutationKind::Insert,
        MutationKind::Update,
        MutationKind::Delete,
    ];

    let meta = WalPartMeta {
        kind: WalPartKind::Cdc,
        row_count: 4,
        rows: kinds
            .iter()
            .enumerate()
            .map(|(i, &k)| WalRowMeta {
                mutation: k,
                event_id: vec![i as u8],
                order_token: vec![0, 0, 0, i as u8],
            })
            .collect(),
    };

    let bytes = bincode::serialize(&meta).unwrap();
    let decoded: WalPartMeta = bincode::deserialize(&bytes).unwrap();

    assert_eq!(decoded.rows[0].mutation, MutationKind::Snapshot);
    assert_eq!(decoded.rows[1].mutation, MutationKind::Insert);
    assert_eq!(decoded.rows[2].mutation, MutationKind::Update);
    assert_eq!(decoded.rows[3].mutation, MutationKind::Delete);
}
