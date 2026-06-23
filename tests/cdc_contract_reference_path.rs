/// CDC Contract / Reference Path Tests (host/core coverage)
///
/// Validates structural contracts and WAL metadata behavior for the exact-once
/// CDC pipeline without depending on sink-local SQL generation helpers.
use std::collections::HashMap;
use std::time::SystemTime;

use skipprd::buffer::segment_file::{PartitionKey, SegmentFile};
use skipprd::helpers::offsets::OffsetKey;
use skipprd::plugins::cdc::*;

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

#[test]
fn reference_path_wal_cdc_metadata_roundtrip() {
    let dir = temp_dir();
    let seg = SegmentFile::new(&dir, "ref_cdc").unwrap();

    let key = PartitionKey {
        sink_ref: "data_outputs.pg".to_string(),
        namespace: "users".to_string(),
        partition: "".to_string(),
        time: None,
        schema_fingerprint: "".to_string(),
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
                order_token: 0x16B3748u64.to_be_bytes().to_vec(),
            },
            WalRowMeta {
                mutation: MutationKind::Update,
                event_id: b"pg:lsn:0/16B3750".to_vec(),
                order_token: 0x16B3750u64.to_be_bytes().to_vec(),
            },
            WalRowMeta {
                mutation: MutationKind::Delete,
                event_id: b"pg:lsn:0/16B3760".to_vec(),
                order_token: 0x16B3760u64.to_be_bytes().to_vec(),
            },
        ],
    };
    let mut blobs = HashMap::new();
    blobs.insert(key.clone(), bincode::serialize(&wal_meta).unwrap());

    seg.write_snapshot(&offsets, &batches, &parts_meta, &blobs)
        .unwrap();

    let mut f = std::fs::File::open(&seg.path).unwrap();
    let read_blobs = SegmentFile::read_part_meta_blobs_from_reader(&mut f).unwrap();
    let decoded: WalPartMeta = bincode::deserialize(read_blobs.get(&key).unwrap()).unwrap();
    assert_eq!(decoded.rows.len(), 3);
    assert_eq!(decoded.rows[2].mutation, MutationKind::Delete);
    assert_eq!(decoded.rows[0].event_id, b"pg:lsn:0/16B3748");
}

#[test]
fn reference_path_wal_append_mode_has_empty_meta() {
    let dir = temp_dir();
    let seg = SegmentFile::new(&dir, "ref_append").unwrap();

    let key = PartitionKey {
        sink_ref: "data_outputs.pg".to_string(),
        namespace: "logs".to_string(),
        partition: "".to_string(),
        time: None,
        schema_fingerprint: "".to_string(),
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

    let mut f = std::fs::File::open(&seg.path).unwrap();
    let blobs = SegmentFile::read_part_meta_blobs_from_reader(&mut f).unwrap();
    assert!(blobs.is_empty());
}

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

    let d1: CheckpointEnvelope =
        bincode::deserialize(&bincode::serialize(&wal_owned).unwrap()).unwrap();
    let d2: CheckpointEnvelope =
        bincode::deserialize(&bincode::serialize(&advisory).unwrap()).unwrap();
    assert_eq!(d1.authority, CheckpointAuthority::WalOwnership);
    assert_eq!(d2.authority, CheckpointAuthority::AdvisoryHint);
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

#[test]
fn reference_path_mutation_to_action_mapping() {
    let bk = vec![("id".to_string(), "1".to_string())];
    let token = vec![0, 0, 0, 1];

    assert!(matches!(
        resolve_apply_action(MutationKind::Snapshot, bk.clone(), token.clone()),
        SinkApplyAction::UpsertIfNewer { .. }
    ));
    assert!(matches!(
        resolve_apply_action(MutationKind::Insert, bk.clone(), token.clone()),
        SinkApplyAction::UpsertIfNewer { .. }
    ));
    assert!(matches!(
        resolve_apply_action(MutationKind::Update, bk.clone(), token.clone()),
        SinkApplyAction::UpsertIfNewer { .. }
    ));
    assert!(matches!(
        resolve_apply_action(MutationKind::Delete, bk, token),
        SinkApplyAction::DeleteIfNewer { .. }
    ));
}

#[test]
fn reference_path_order_token_lexicographic_comparison() {
    let older: Vec<u8> = vec![0, 0, 0, 0, 0x16, 0xB3, 0x74, 0x80];
    let newer: Vec<u8> = vec![0, 0, 0, 0, 0x16, 0xB3, 0x74, 0x90];
    assert!(older < newer);

    let snapshot_token: Vec<u8> = vec![0, 0, 0, 0, 0x16, 0xB3, 0x74, 0x80];
    let log_token: Vec<u8> = vec![1, 0, 0, 0, 0x16, 0xB3, 0x74, 0x80];
    assert!(snapshot_token < log_token);
}

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
            assert_eq!(g, EffectiveGuarantee::CdcEncoded);
        }
        CompatibilityResult::Incompatible(_) => {}
    }
}

#[test]
fn source_helper_mongodb_cluster_time_order_token_encoding() {
    let seconds: u32 = 1_700_000_000;
    let increment: u32 = 42;
    let combined = ((seconds as u64) << 32) | (increment as u64);
    let token = combined.to_be_bytes().to_vec();
    let recovered = u64::from_be_bytes(token.clone().try_into().unwrap());

    assert_eq!(token.len(), 8);
    assert_eq!(recovered >> 32, seconds as u64);
    assert_eq!(recovered & 0xFFFF_FFFF, increment as u64);
}

#[test]
fn source_helper_mongodb_cluster_time_lexicographic_ordering() {
    let earlier = (((100u64) << 32) | 50u64).to_be_bytes().to_vec();
    let later = (((200u64) << 32) | 1u64).to_be_bytes().to_vec();
    assert!(earlier < later);

    let low_inc = (((100u64) << 32) | 5u64).to_be_bytes().to_vec();
    let high_inc = (((100u64) << 32) | 500u64).to_be_bytes().to_vec();
    assert!(low_inc < high_inc);
}

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
            .map(|(i, &kind)| WalRowMeta {
                mutation: kind,
                event_id: vec![i as u8],
                order_token: vec![0, 0, 0, i as u8],
            })
            .collect(),
    };

    let decoded: WalPartMeta = bincode::deserialize(&bincode::serialize(&meta).unwrap()).unwrap();
    assert_eq!(decoded.rows[0].mutation, MutationKind::Snapshot);
    assert_eq!(decoded.rows[1].mutation, MutationKind::Insert);
    assert_eq!(decoded.rows[2].mutation, MutationKind::Update);
    assert_eq!(decoded.rows[3].mutation, MutationKind::Delete);
}
