use serde_derive::{Deserialize, Serialize};
use std::sync::RwLock;

static CDC_NAMESPACE_CONTRACT: RwLock<Option<NamespaceContract>> = RwLock::new(None);

/// Store the pipeline's CDC contract so the compactor can populate
/// `SyncContext.contract` without passing it through every layer.
pub fn set_global_cdc_contract(contract: Option<NamespaceContract>) {
    *CDC_NAMESPACE_CONTRACT.write().unwrap() = contract;
}

/// Retrieve the CDC contract for the current pipeline.
pub fn get_global_cdc_contract() -> Option<NamespaceContract> {
    CDC_NAMESPACE_CONTRACT.read().unwrap().clone()
}

// ---------------------------------------------------------------------------
// Checkpoint authority model
// ---------------------------------------------------------------------------

/// Classifies the provenance of a checkpoint value so callers can distinguish
/// WAL-authoritative ownership state from advisory operational hints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckpointAuthority {
    /// Derived from a committed WAL segment (`.seg` + `.seg.commit`).
    /// This is the sole proof that Skippr durably owns the corresponding
    /// source events. Recovery rebuilds these from committed segments.
    WalOwnership,
    /// Written out-of-band for operational performance (e.g. S3 list
    /// short-circuiting). May advance ahead of WAL commits. Must never
    /// be the only proof of source event ownership.
    AdvisoryHint,
}

// ---------------------------------------------------------------------------
// Generic checkpoint envelope
// ---------------------------------------------------------------------------

/// Versioned, source-agnostic checkpoint envelope stored in the checkpoint DB.
///
/// Each source plugin defines its own typed payload and serialization rules.
/// The core only stores and returns opaque bytes plus enough metadata to reject
/// incompatible or stale payload versions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointEnvelope {
    pub authority: CheckpointAuthority,
    pub kind: CheckpointKind,
    pub payload_version: u32,
    pub payload_bytes: Vec<u8>,
}

/// The family of checkpoint being stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckpointKind {
    /// Steady-state source resume position.
    SourceResume,
    /// Bootstrap / initial-snapshot progress marker.
    BootstrapProgress,
    /// Source-specific anchor metadata (e.g. log position captured before
    /// snapshot for gap-free handoff).
    BootstrapAnchor,
    /// Advisory source-side progress hint (e.g. S3 list-filter state).
    AdvisoryProgress,
}

// ---------------------------------------------------------------------------
// Per-source checkpoint payloads
// ---------------------------------------------------------------------------

/// Postgres checkpoint — stores the WAL LSN for resume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostgresCheckpoint {
    pub lsn: u64,
    pub slot_name: String,
}

/// MySQL checkpoint — stores binlog file and position.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MysqlCheckpoint {
    pub binlog_file: String,
    pub binlog_position: u64,
}

/// MongoDB checkpoint — stores the resume token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MongodbCheckpoint {
    pub resume_token: Vec<u8>,
}

/// DynamoDB checkpoint — stores stream position per shard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DynamodbCheckpoint {
    pub table_name: String,
    pub stream_arn: String,
    pub shard_id: String,
    pub sequence_number: String,
}

/// Kafka checkpoint — stores topic/partition/offset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KafkaCheckpoint {
    pub topic: String,
    pub partition: i32,
    pub offset: i64,
}

impl CheckpointEnvelope {
    /// Build a checkpoint envelope from a typed payload.
    pub fn from_payload<T: serde::Serialize>(
        authority: CheckpointAuthority,
        kind: CheckpointKind,
        payload_version: u32,
        payload: &T,
    ) -> Result<Self, bincode::Error> {
        Ok(Self {
            authority,
            kind,
            payload_version,
            payload_bytes: bincode::serialize(payload)?,
        })
    }

    /// Deserialize the payload into the expected type.
    pub fn into_payload<T: serde::de::DeserializeOwned>(&self) -> Result<T, bincode::Error> {
        bincode::deserialize(&self.payload_bytes)
    }
}

// ---------------------------------------------------------------------------
// Mutation fidelity
// ---------------------------------------------------------------------------

/// The kind of mutation a source event represents.
/// Preserved with full fidelity in the WAL. Sinks may collapse
/// `Snapshot`, `Insert`, and `Update` into upserts at apply time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MutationKind {
    Snapshot,
    Insert,
    Update,
    Delete,
}

// ---------------------------------------------------------------------------
// Source order model
// ---------------------------------------------------------------------------

/// Declares how a source's `order_token` values relate to each other.
/// Exact-once final-state sinks require either `GlobalTotalOrder` or
/// `StableKeyScopedOrder`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceOrderModel {
    /// All events from this source can be placed in a single total order.
    /// Any two `order_token` values are directly comparable regardless of
    /// business key or partition.
    GlobalTotalOrder,
    /// All mutations for a given business key remain inside a stable
    /// comparable scope, so `order_token` values are meaningful per-key
    /// even without a single global order across the whole source.
    StableKeyScopedOrder,
    /// Source cannot produce a clean comparable `order_token` for
    /// final-state reconciliation. CDC-encoded sinks may still land
    /// events, but exact-once final-state sinks must reject this source.
    UnsupportedForFinalState,
}

// ---------------------------------------------------------------------------
// WAL row metadata
// ---------------------------------------------------------------------------

/// Per-row CDC metadata carried as a sidecar blob in each WAL `PART`.
/// Aligned 1:1 with the Arrow IPC row order in the same partition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalRowMeta {
    pub mutation: MutationKind,
    /// Source-defined equality token for intake exact-once identity.
    /// Two events with the same `event_id` are the same source event.
    pub event_id: Vec<u8>,
    /// Source-defined comparable token for stale-write rejection.
    /// Must be lexicographically sortable within the source's declared
    /// `SourceOrderModel` scope.
    pub order_token: Vec<u8>,
}

/// Per-partition CDC metadata blob written alongside each WAL `PART`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalPartMeta {
    pub kind: WalPartKind,
    pub row_count: u64,
    pub rows: Vec<WalRowMeta>,
}

/// Distinguishes legacy append-mode partitions from CDC-aware partitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WalPartKind {
    /// Legacy append mode. `rows` is empty and Arrow payload is unchanged.
    Append,
    /// CDC mode. `rows.len() == row_count` and metadata is aligned 1:1
    /// with row order in the Arrow IPC stream.
    Cdc,
}

// ---------------------------------------------------------------------------
// Source capability declarations
// ---------------------------------------------------------------------------

/// Exhaustive source capability descriptor. Every source connector must
/// declare one of these at compile time. The runtime planner uses it to
/// validate compatibility with the selected sink and namespace contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceCapability {
    pub name: &'static str,
    pub guarantee_tier: SourceGuaranteeTier,
    pub checkpoint_style: SourceCheckpointStyle,
    pub bootstrap_style: SourceBootstrapStyle,
    pub order_model: SourceOrderModel,
    pub supports_deletes: bool,
    pub event_id_semantics: EventIdSemantics,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceGuaranteeTier {
    SnapshotThenLogExactOnce,
    LogStreamExactOnce,
    EventIdentityEligible,
    IncrementalOnly,
    UserSuppliedIdentityOnly,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceCheckpointStyle {
    /// Source provides a log-native resume position (LSN, offset, etc.).
    LogNative,
    /// Source uses cursor-based incremental queries.
    CursorBased,
    /// Source uses object/path identity (files, S3 keys).
    ObjectIdentity,
    /// Source uses message-level identity (SQS message id, etc.).
    MessageIdentity,
    /// Source has no meaningful checkpoint.
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceBootstrapStyle {
    /// Source can produce an anchored snapshot tied to a log resume point.
    AnchoredSnapshot,
    /// Source performs a full scan without log anchoring.
    FullScan,
    /// Source is stream-only with no snapshot capability.
    StreamOnly,
    /// Source does not support bootstrap.
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventIdSemantics {
    /// Source provides a native log position as event identity.
    LogPosition,
    /// Source provides stable message-level identity.
    MessageId,
    /// Source provides object/path identity.
    ObjectPath,
    /// Caller must supply identity; source has none.
    UserSupplied,
    /// No meaningful event identity.
    None,
}

// ---------------------------------------------------------------------------
// Sink capability declarations
// ---------------------------------------------------------------------------

/// Exhaustive sink capability descriptor. Every sink connector must declare
/// one of these at compile time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SinkCapability {
    pub name: &'static str,
    pub guarantee_tier: SinkGuaranteeTier,
    pub can_manage_skippr_columns: bool,
    pub can_maintain_tombstone_tables: bool,
    pub can_compare_order_tokens: bool,
    pub supports_transactions: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SinkGuaranteeTier {
    /// Full insert/update/delete reconciliation with exactly-once visible
    /// results via per-slice transactions or deterministic idempotent merge.
    ExactOnceCdcEligible,
    /// Can faithfully land CDC payloads but does not natively reconcile
    /// final table state.
    CdcEncodedOnly,
    /// Append-only; no CDC support.
    AppendOnly,
    /// Must never claim any CDC support.
    Unsupported,
}

// ---------------------------------------------------------------------------
// Namespace contract
// ---------------------------------------------------------------------------

/// Per-namespace contract that ties source, sink, and schema together.
/// The `effective_guarantee` is derived from source/sink capabilities, never
/// from user config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamespaceContract {
    pub namespace: String,
    pub business_key_columns: Vec<String>,
    pub effective_guarantee: EffectiveGuarantee,
}

/// The strongest CDC semantics the runtime will enforce for this pipeline.
/// Derived deterministically from the source/sink capability pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffectiveGuarantee {
    ExactOnceFinalState,
    CdcEncoded,
}

// ---------------------------------------------------------------------------
// Guarantee derivation and validation
// ---------------------------------------------------------------------------

/// Result of deriving and validating the effective guarantee.
#[derive(Debug)]
pub enum CompatibilityResult {
    /// The pair supports CDC and all prerequisites are met.
    Compatible(EffectiveGuarantee),
    /// The pair cannot satisfy its strongest achievable CDC mode.
    Incompatible(Vec<String>),
}

/// Derive the strongest guarantee the source/sink pair supports, then
/// validate that all prerequisites are met. Fails fast with explicit reasons
/// when the pair cannot satisfy its own maximum achievable CDC semantics.
pub fn derive_and_validate(
    source: &SourceCapability,
    sink: &SinkCapability,
    namespace: &str,
    business_key_columns: &[String],
) -> CompatibilityResult {
    let source_can_cdc = matches!(
        source.guarantee_tier,
        SourceGuaranteeTier::SnapshotThenLogExactOnce
            | SourceGuaranteeTier::LogStreamExactOnce
            | SourceGuaranteeTier::EventIdentityEligible
    );

    if !source_can_cdc {
        return CompatibilityResult::Incompatible(vec![format!(
            "source '{}' (tier {:?}) cannot produce CDC metadata",
            source.name, source.guarantee_tier
        )]);
    }

    let sink_can_cdc = matches!(
        sink.guarantee_tier,
        SinkGuaranteeTier::ExactOnceCdcEligible | SinkGuaranteeTier::CdcEncodedOnly
    );

    if !sink_can_cdc {
        return CompatibilityResult::Incompatible(vec![format!(
            "sink '{}' (tier {:?}) cannot accept CDC payloads",
            sink.name, sink.guarantee_tier
        )]);
    }

    let source_exact_once = matches!(
        source.guarantee_tier,
        SourceGuaranteeTier::SnapshotThenLogExactOnce | SourceGuaranteeTier::LogStreamExactOnce
    );
    let source_order_ok = source.order_model != SourceOrderModel::UnsupportedForFinalState;
    let sink_exact_once = sink.guarantee_tier == SinkGuaranteeTier::ExactOnceCdcEligible
        && sink.can_manage_skippr_columns
        && sink.can_maintain_tombstone_tables
        && sink.can_compare_order_tokens;

    if source_exact_once && source_order_ok && sink_exact_once {
        if business_key_columns.is_empty() {
            return CompatibilityResult::Incompatible(vec![format!(
                "namespace '{}' requires business_key_columns for ExactOnceFinalState \
                 (source '{}' + sink '{}' are both exact-once capable)",
                namespace, source.name, sink.name
            )]);
        }
        return CompatibilityResult::Compatible(EffectiveGuarantee::ExactOnceFinalState);
    }

    CompatibilityResult::Compatible(EffectiveGuarantee::CdcEncoded)
}


// ---------------------------------------------------------------------------
// Sync context
// ---------------------------------------------------------------------------

/// Context passed to `DataSink::sync` when the partition carries CDC metadata.
/// Sinks branch on the presence of this context to decide between append-only
/// INSERT and CDC-aware apply (upsert/delete with order-token guards).
#[derive(Debug, Clone)]
pub struct SyncContext {
    pub part_meta: WalPartMeta,
    /// Namespace contract describing business keys and the enforced guarantee.
    /// `None` when the pipeline has not configured a CDC contract; sinks
    /// receiving CDC metadata without a contract should fall back to append.
    pub contract: Option<NamespaceContract>,
}

// ---------------------------------------------------------------------------
// Sink apply types
// ---------------------------------------------------------------------------

/// Describes how a CDC mutation should be applied to a target table and its
/// companion tombstone table in a single transaction.
#[derive(Debug, Clone)]
pub enum SinkApplyAction {
    /// Upsert the row into the target table if the incoming `order_token`
    /// is newer than both the existing live-row token and any matching
    /// tombstone token. If the upsert wins over a tombstone, remove the
    /// stale tombstone in the same transaction.
    UpsertIfNewer {
        business_key: Vec<(String, String)>,
        order_token: Vec<u8>,
    },
    /// Delete the live row if the incoming `order_token` is newer than the
    /// live-row token, then upsert the tombstone-table row with the delete
    /// token in the same transaction.
    DeleteIfNewer {
        business_key: Vec<(String, String)>,
        order_token: Vec<u8>,
    },
}

/// Resolve a WAL row mutation into a sink apply action.
pub fn resolve_apply_action(
    mutation: MutationKind,
    business_key: Vec<(String, String)>,
    order_token: Vec<u8>,
) -> SinkApplyAction {
    match mutation {
        MutationKind::Delete => SinkApplyAction::DeleteIfNewer {
            business_key,
            order_token,
        },
        MutationKind::Snapshot | MutationKind::Insert | MutationKind::Update => {
            SinkApplyAction::UpsertIfNewer {
                business_key,
                order_token,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Per-connector capability declarations
// ---------------------------------------------------------------------------

pub mod source_capabilities {
    use super::*;

    pub const POSTGRES: SourceCapability = SourceCapability {
        name: "Postgres",
        guarantee_tier: SourceGuaranteeTier::SnapshotThenLogExactOnce,
        checkpoint_style: SourceCheckpointStyle::LogNative,
        bootstrap_style: SourceBootstrapStyle::AnchoredSnapshot,
        order_model: SourceOrderModel::GlobalTotalOrder,
        supports_deletes: true,
        event_id_semantics: EventIdSemantics::LogPosition,
    };

    pub const MYSQL: SourceCapability = SourceCapability {
        name: "Mysql",
        guarantee_tier: SourceGuaranteeTier::SnapshotThenLogExactOnce,
        checkpoint_style: SourceCheckpointStyle::LogNative,
        bootstrap_style: SourceBootstrapStyle::AnchoredSnapshot,
        order_model: SourceOrderModel::GlobalTotalOrder,
        supports_deletes: true,
        event_id_semantics: EventIdSemantics::LogPosition,
    };

    pub const MSSQL: SourceCapability = SourceCapability {
        name: "Mssql",
        guarantee_tier: SourceGuaranteeTier::SnapshotThenLogExactOnce,
        checkpoint_style: SourceCheckpointStyle::LogNative,
        bootstrap_style: SourceBootstrapStyle::AnchoredSnapshot,
        order_model: SourceOrderModel::GlobalTotalOrder,
        supports_deletes: true,
        event_id_semantics: EventIdSemantics::LogPosition,
    };

    pub const MONGODB: SourceCapability = SourceCapability {
        name: "Mongodb",
        guarantee_tier: SourceGuaranteeTier::SnapshotThenLogExactOnce,
        checkpoint_style: SourceCheckpointStyle::LogNative,
        bootstrap_style: SourceBootstrapStyle::AnchoredSnapshot,
        order_model: SourceOrderModel::StableKeyScopedOrder,
        supports_deletes: true,
        event_id_semantics: EventIdSemantics::LogPosition,
    };

    pub const DYNAMODB: SourceCapability = SourceCapability {
        name: "Dynamodb",
        guarantee_tier: SourceGuaranteeTier::SnapshotThenLogExactOnce,
        checkpoint_style: SourceCheckpointStyle::LogNative,
        bootstrap_style: SourceBootstrapStyle::AnchoredSnapshot,
        order_model: SourceOrderModel::StableKeyScopedOrder,
        supports_deletes: true,
        event_id_semantics: EventIdSemantics::LogPosition,
    };

    pub const DELTA_LAKE: SourceCapability = SourceCapability {
        name: "DeltaLake",
        guarantee_tier: SourceGuaranteeTier::SnapshotThenLogExactOnce,
        checkpoint_style: SourceCheckpointStyle::LogNative,
        bootstrap_style: SourceBootstrapStyle::AnchoredSnapshot,
        order_model: SourceOrderModel::GlobalTotalOrder,
        supports_deletes: true,
        event_id_semantics: EventIdSemantics::LogPosition,
    };

    pub const KAFKA: SourceCapability = SourceCapability {
        name: "Kafka",
        guarantee_tier: SourceGuaranteeTier::LogStreamExactOnce,
        checkpoint_style: SourceCheckpointStyle::LogNative,
        bootstrap_style: SourceBootstrapStyle::StreamOnly,
        order_model: SourceOrderModel::StableKeyScopedOrder,
        supports_deletes: true,
        event_id_semantics: EventIdSemantics::LogPosition,
    };

    pub const KINESIS: SourceCapability = SourceCapability {
        name: "Kinesis",
        guarantee_tier: SourceGuaranteeTier::LogStreamExactOnce,
        checkpoint_style: SourceCheckpointStyle::LogNative,
        bootstrap_style: SourceBootstrapStyle::StreamOnly,
        order_model: SourceOrderModel::StableKeyScopedOrder,
        supports_deletes: false,
        event_id_semantics: EventIdSemantics::LogPosition,
    };

    pub const SQS: SourceCapability = SourceCapability {
        name: "Sqs",
        guarantee_tier: SourceGuaranteeTier::EventIdentityEligible,
        checkpoint_style: SourceCheckpointStyle::MessageIdentity,
        bootstrap_style: SourceBootstrapStyle::StreamOnly,
        order_model: SourceOrderModel::UnsupportedForFinalState,
        supports_deletes: false,
        event_id_semantics: EventIdSemantics::MessageId,
    };

    pub const SNS: SourceCapability = SourceCapability {
        name: "Sns",
        guarantee_tier: SourceGuaranteeTier::EventIdentityEligible,
        checkpoint_style: SourceCheckpointStyle::MessageIdentity,
        bootstrap_style: SourceBootstrapStyle::StreamOnly,
        order_model: SourceOrderModel::UnsupportedForFinalState,
        supports_deletes: false,
        event_id_semantics: EventIdSemantics::MessageId,
    };

    pub const EVENTBRIDGE: SourceCapability = SourceCapability {
        name: "Eventbridge",
        guarantee_tier: SourceGuaranteeTier::EventIdentityEligible,
        checkpoint_style: SourceCheckpointStyle::MessageIdentity,
        bootstrap_style: SourceBootstrapStyle::StreamOnly,
        order_model: SourceOrderModel::UnsupportedForFinalState,
        supports_deletes: false,
        event_id_semantics: EventIdSemantics::MessageId,
    };

    pub const FILE: SourceCapability = SourceCapability {
        name: "File",
        guarantee_tier: SourceGuaranteeTier::IncrementalOnly,
        checkpoint_style: SourceCheckpointStyle::ObjectIdentity,
        bootstrap_style: SourceBootstrapStyle::FullScan,
        order_model: SourceOrderModel::UnsupportedForFinalState,
        supports_deletes: false,
        event_id_semantics: EventIdSemantics::ObjectPath,
    };

    pub const S3: SourceCapability = SourceCapability {
        name: "S3",
        guarantee_tier: SourceGuaranteeTier::IncrementalOnly,
        checkpoint_style: SourceCheckpointStyle::ObjectIdentity,
        bootstrap_style: SourceBootstrapStyle::FullScan,
        order_model: SourceOrderModel::UnsupportedForFinalState,
        supports_deletes: false,
        event_id_semantics: EventIdSemantics::ObjectPath,
    };

    pub const SFTP: SourceCapability = SourceCapability {
        name: "Sftp",
        guarantee_tier: SourceGuaranteeTier::IncrementalOnly,
        checkpoint_style: SourceCheckpointStyle::ObjectIdentity,
        bootstrap_style: SourceBootstrapStyle::FullScan,
        order_model: SourceOrderModel::UnsupportedForFinalState,
        supports_deletes: false,
        event_id_semantics: EventIdSemantics::ObjectPath,
    };

    pub const HTTP_CLIENT: SourceCapability = SourceCapability {
        name: "HttpClient",
        guarantee_tier: SourceGuaranteeTier::IncrementalOnly,
        checkpoint_style: SourceCheckpointStyle::CursorBased,
        bootstrap_style: SourceBootstrapStyle::FullScan,
        order_model: SourceOrderModel::UnsupportedForFinalState,
        supports_deletes: false,
        event_id_semantics: EventIdSemantics::None,
    };

    pub const CLICKHOUSE: SourceCapability = SourceCapability {
        name: "Clickhouse",
        guarantee_tier: SourceGuaranteeTier::IncrementalOnly,
        checkpoint_style: SourceCheckpointStyle::CursorBased,
        bootstrap_style: SourceBootstrapStyle::FullScan,
        order_model: SourceOrderModel::UnsupportedForFinalState,
        supports_deletes: false,
        event_id_semantics: EventIdSemantics::None,
    };

    pub const REDSHIFT: SourceCapability = SourceCapability {
        name: "Redshift",
        guarantee_tier: SourceGuaranteeTier::IncrementalOnly,
        checkpoint_style: SourceCheckpointStyle::CursorBased,
        bootstrap_style: SourceBootstrapStyle::FullScan,
        order_model: SourceOrderModel::UnsupportedForFinalState,
        supports_deletes: false,
        event_id_semantics: EventIdSemantics::None,
    };

    pub const MOTHERDUCK: SourceCapability = SourceCapability {
        name: "Motherduck",
        guarantee_tier: SourceGuaranteeTier::IncrementalOnly,
        checkpoint_style: SourceCheckpointStyle::CursorBased,
        bootstrap_style: SourceBootstrapStyle::FullScan,
        order_model: SourceOrderModel::UnsupportedForFinalState,
        supports_deletes: false,
        event_id_semantics: EventIdSemantics::None,
    };

    pub const AMQP: SourceCapability = SourceCapability {
        name: "Amqp",
        guarantee_tier: SourceGuaranteeTier::UserSuppliedIdentityOnly,
        checkpoint_style: SourceCheckpointStyle::MessageIdentity,
        bootstrap_style: SourceBootstrapStyle::StreamOnly,
        order_model: SourceOrderModel::UnsupportedForFinalState,
        supports_deletes: false,
        event_id_semantics: EventIdSemantics::UserSupplied,
    };

    pub const MQTT: SourceCapability = SourceCapability {
        name: "Mqtt",
        guarantee_tier: SourceGuaranteeTier::UserSuppliedIdentityOnly,
        checkpoint_style: SourceCheckpointStyle::MessageIdentity,
        bootstrap_style: SourceBootstrapStyle::StreamOnly,
        order_model: SourceOrderModel::UnsupportedForFinalState,
        supports_deletes: false,
        event_id_semantics: EventIdSemantics::UserSupplied,
    };

    pub const HTTP_SERVER: SourceCapability = SourceCapability {
        name: "HttpServer",
        guarantee_tier: SourceGuaranteeTier::UserSuppliedIdentityOnly,
        checkpoint_style: SourceCheckpointStyle::None,
        bootstrap_style: SourceBootstrapStyle::StreamOnly,
        order_model: SourceOrderModel::UnsupportedForFinalState,
        supports_deletes: false,
        event_id_semantics: EventIdSemantics::UserSupplied,
    };

    pub const WEBSOCKET: SourceCapability = SourceCapability {
        name: "Websocket",
        guarantee_tier: SourceGuaranteeTier::UserSuppliedIdentityOnly,
        checkpoint_style: SourceCheckpointStyle::None,
        bootstrap_style: SourceBootstrapStyle::StreamOnly,
        order_model: SourceOrderModel::UnsupportedForFinalState,
        supports_deletes: false,
        event_id_semantics: EventIdSemantics::UserSupplied,
    };

    pub const SOCKET: SourceCapability = SourceCapability {
        name: "Socket",
        guarantee_tier: SourceGuaranteeTier::UserSuppliedIdentityOnly,
        checkpoint_style: SourceCheckpointStyle::None,
        bootstrap_style: SourceBootstrapStyle::StreamOnly,
        order_model: SourceOrderModel::UnsupportedForFinalState,
        supports_deletes: false,
        event_id_semantics: EventIdSemantics::UserSupplied,
    };

    pub const STDIN: SourceCapability = SourceCapability {
        name: "Stdin",
        guarantee_tier: SourceGuaranteeTier::Unsupported,
        checkpoint_style: SourceCheckpointStyle::None,
        bootstrap_style: SourceBootstrapStyle::None,
        order_model: SourceOrderModel::UnsupportedForFinalState,
        supports_deletes: false,
        event_id_semantics: EventIdSemantics::None,
    };

    pub const STATSD: SourceCapability = SourceCapability {
        name: "Statsd",
        guarantee_tier: SourceGuaranteeTier::Unsupported,
        checkpoint_style: SourceCheckpointStyle::None,
        bootstrap_style: SourceBootstrapStyle::None,
        order_model: SourceOrderModel::UnsupportedForFinalState,
        supports_deletes: false,
        event_id_semantics: EventIdSemantics::None,
    };

    pub const PCAP: SourceCapability = SourceCapability {
        name: "Pcap",
        guarantee_tier: SourceGuaranteeTier::Unsupported,
        checkpoint_style: SourceCheckpointStyle::None,
        bootstrap_style: SourceBootstrapStyle::None,
        order_model: SourceOrderModel::UnsupportedForFinalState,
        supports_deletes: false,
        event_id_semantics: EventIdSemantics::None,
    };

    /// Look up a source capability by plugin name.
    pub fn by_name(name: &str) -> Option<&'static SourceCapability> {
        match name {
            "Postgres" => Some(&POSTGRES),
            "Mysql" => Some(&MYSQL),
            "Mssql" => Some(&MSSQL),
            "Mongodb" => Some(&MONGODB),
            "Dynamodb" => Some(&DYNAMODB),
            "DeltaLake" => Some(&DELTA_LAKE),
            "Kafka" => Some(&KAFKA),
            "Kinesis" => Some(&KINESIS),
            "Sqs" => Some(&SQS),
            "Sns" => Some(&SNS),
            "Eventbridge" => Some(&EVENTBRIDGE),
            "File" => Some(&FILE),
            "S3" => Some(&S3),
            "Sftp" => Some(&SFTP),
            "HttpClient" => Some(&HTTP_CLIENT),
            "Clickhouse" => Some(&CLICKHOUSE),
            "Redshift" => Some(&REDSHIFT),
            "Motherduck" => Some(&MOTHERDUCK),
            "Amqp" => Some(&AMQP),
            "Mqtt" => Some(&MQTT),
            "HttpServer" => Some(&HTTP_SERVER),
            "Websocket" => Some(&WEBSOCKET),
            "Socket" => Some(&SOCKET),
            "Stdin" => Some(&STDIN),
            "Statsd" => Some(&STATSD),
            "Pcap" => Some(&PCAP),
            _ => None,
        }
    }
}

pub mod sink_capabilities {
    use super::*;

    pub const POSTGRES: SinkCapability = SinkCapability {
        name: "Postgres",
        guarantee_tier: SinkGuaranteeTier::ExactOnceCdcEligible,
        can_manage_skippr_columns: true,
        can_maintain_tombstone_tables: true,
        can_compare_order_tokens: true,
        supports_transactions: true,
    };

    pub const SNOWFLAKE: SinkCapability = SinkCapability {
        name: "Snowflake",
        guarantee_tier: SinkGuaranteeTier::ExactOnceCdcEligible,
        can_manage_skippr_columns: true,
        can_maintain_tombstone_tables: true,
        can_compare_order_tokens: true,
        supports_transactions: true,
    };

    pub const BIGQUERY: SinkCapability = SinkCapability {
        name: "Bigquery",
        guarantee_tier: SinkGuaranteeTier::ExactOnceCdcEligible,
        can_manage_skippr_columns: true,
        can_maintain_tombstone_tables: true,
        can_compare_order_tokens: true,
        supports_transactions: true,
    };

    pub const REDSHIFT: SinkCapability = SinkCapability {
        name: "Redshift",
        guarantee_tier: SinkGuaranteeTier::ExactOnceCdcEligible,
        can_manage_skippr_columns: true,
        can_maintain_tombstone_tables: true,
        can_compare_order_tokens: true,
        supports_transactions: true,
    };

    pub const DATABRICKS: SinkCapability = SinkCapability {
        name: "Databricks",
        guarantee_tier: SinkGuaranteeTier::ExactOnceCdcEligible,
        can_manage_skippr_columns: true,
        can_maintain_tombstone_tables: true,
        can_compare_order_tokens: true,
        supports_transactions: true,
    };

    pub const MOTHERDUCK: SinkCapability = SinkCapability {
        name: "Motherduck",
        guarantee_tier: SinkGuaranteeTier::ExactOnceCdcEligible,
        can_manage_skippr_columns: true,
        can_maintain_tombstone_tables: true,
        can_compare_order_tokens: true,
        supports_transactions: true,
    };

    pub const CLICKHOUSE: SinkCapability = SinkCapability {
        name: "Clickhouse",
        guarantee_tier: SinkGuaranteeTier::ExactOnceCdcEligible,
        can_manage_skippr_columns: true,
        can_maintain_tombstone_tables: true,
        can_compare_order_tokens: true,
        supports_transactions: false,
    };

    pub const SYNAPSE: SinkCapability = SinkCapability {
        name: "Synapse",
        guarantee_tier: SinkGuaranteeTier::ExactOnceCdcEligible,
        can_manage_skippr_columns: true,
        can_maintain_tombstone_tables: true,
        can_compare_order_tokens: true,
        supports_transactions: true,
    };

    pub const ATHENA: SinkCapability = SinkCapability {
        name: "Athena",
        guarantee_tier: SinkGuaranteeTier::CdcEncodedOnly,
        can_manage_skippr_columns: false,
        can_maintain_tombstone_tables: false,
        can_compare_order_tokens: false,
        supports_transactions: false,
    };

    pub const S3: SinkCapability = SinkCapability {
        name: "S3",
        guarantee_tier: SinkGuaranteeTier::CdcEncodedOnly,
        can_manage_skippr_columns: false,
        can_maintain_tombstone_tables: false,
        can_compare_order_tokens: false,
        supports_transactions: false,
    };

    pub const GCS: SinkCapability = SinkCapability {
        name: "Gcs",
        guarantee_tier: SinkGuaranteeTier::CdcEncodedOnly,
        can_manage_skippr_columns: false,
        can_maintain_tombstone_tables: false,
        can_compare_order_tokens: false,
        supports_transactions: false,
    };

    pub const AZURE_BLOB: SinkCapability = SinkCapability {
        name: "AzureBlob",
        guarantee_tier: SinkGuaranteeTier::CdcEncodedOnly,
        can_manage_skippr_columns: false,
        can_maintain_tombstone_tables: false,
        can_compare_order_tokens: false,
        supports_transactions: false,
    };

    pub const FILE: SinkCapability = SinkCapability {
        name: "File",
        guarantee_tier: SinkGuaranteeTier::CdcEncodedOnly,
        can_manage_skippr_columns: false,
        can_maintain_tombstone_tables: false,
        can_compare_order_tokens: false,
        supports_transactions: false,
    };

    pub const SFTP: SinkCapability = SinkCapability {
        name: "Sftp",
        guarantee_tier: SinkGuaranteeTier::CdcEncodedOnly,
        can_manage_skippr_columns: false,
        can_maintain_tombstone_tables: false,
        can_compare_order_tokens: false,
        supports_transactions: false,
    };

    pub const AMQP: SinkCapability = SinkCapability {
        name: "Amqp",
        guarantee_tier: SinkGuaranteeTier::CdcEncodedOnly,
        can_manage_skippr_columns: false,
        can_maintain_tombstone_tables: false,
        can_compare_order_tokens: false,
        supports_transactions: false,
    };

    pub const STDOUT: SinkCapability = SinkCapability {
        name: "Stdout",
        guarantee_tier: SinkGuaranteeTier::Unsupported,
        can_manage_skippr_columns: false,
        can_maintain_tombstone_tables: false,
        can_compare_order_tokens: false,
        supports_transactions: false,
    };

    /// Look up a sink capability by plugin name.
    pub fn by_name(name: &str) -> Option<&'static SinkCapability> {
        match name {
            "Postgres" => Some(&POSTGRES),
            "Snowflake" => Some(&SNOWFLAKE),
            "Bigquery" => Some(&BIGQUERY),
            "Redshift" => Some(&REDSHIFT),
            "Databricks" => Some(&DATABRICKS),
            "Motherduck" => Some(&MOTHERDUCK),
            "Clickhouse" => Some(&CLICKHOUSE),
            "Synapse" => Some(&SYNAPSE),
            "Athena" => Some(&ATHENA),
            "S3" => Some(&S3),
            "Gcs" => Some(&GCS),
            "AzureBlob" => Some(&AZURE_BLOB),
            "File" => Some(&FILE),
            "Sftp" => Some(&SFTP),
            "Amqp" => Some(&AMQP),
            "Stdout" => Some(&STDOUT),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_source_capability_lookup_exhaustive() {
        let names = [
            "Postgres",
            "Mysql",
            "Mssql",
            "Mongodb",
            "Dynamodb",
            "DeltaLake",
            "Kafka",
            "Kinesis",
            "Sqs",
            "Sns",
            "Eventbridge",
            "File",
            "S3",
            "Sftp",
            "HttpClient",
            "Clickhouse",
            "Redshift",
            "Motherduck",
            "Amqp",
            "Mqtt",
            "HttpServer",
            "Websocket",
            "Socket",
            "Stdin",
            "Statsd",
            "Pcap",
        ];
        for name in &names {
            assert!(
                source_capabilities::by_name(name).is_some(),
                "missing source capability for '{}'",
                name
            );
        }
    }

    #[test]
    fn test_sink_capability_lookup_exhaustive() {
        let names = [
            "Postgres",
            "Snowflake",
            "Bigquery",
            "Redshift",
            "Databricks",
            "Motherduck",
            "Clickhouse",
            "Synapse",
            "Athena",
            "S3",
            "Gcs",
            "AzureBlob",
            "File",
            "Sftp",
            "Amqp",
            "Stdout",
        ];
        for name in &names {
            assert!(
                sink_capabilities::by_name(name).is_some(),
                "missing sink capability for '{}'",
                name
            );
        }
    }

    #[test]
    fn test_unknown_source_returns_none() {
        assert!(source_capabilities::by_name("NonExistent").is_none());
    }

    #[test]
    fn test_unknown_sink_returns_none() {
        assert!(sink_capabilities::by_name("NonExistent").is_none());
    }

    #[test]
    fn test_postgres_to_postgres_derives_exact_once() {
        let source = source_capabilities::POSTGRES;
        let sink = sink_capabilities::POSTGRES;
        let keys = vec!["id".to_string()];
        match derive_and_validate(&source, &sink, "users", &keys) {
            CompatibilityResult::Compatible(g) => {
                assert_eq!(g, EffectiveGuarantee::ExactOnceFinalState);
            }
            CompatibilityResult::Incompatible(reasons) => {
                panic!("expected compatible, got: {:?}", reasons);
            }
        }
    }

    #[test]
    fn test_stdin_to_postgres_incompatible() {
        let source = source_capabilities::STDIN;
        let sink = sink_capabilities::POSTGRES;
        let keys = vec!["id".to_string()];
        match derive_and_validate(&source, &sink, "events", &keys) {
            CompatibilityResult::Compatible(_) => {
                panic!("expected incompatible for stdin source");
            }
            CompatibilityResult::Incompatible(reasons) => {
                assert!(!reasons.is_empty());
            }
        }
    }

    #[test]
    fn test_postgres_to_s3_derives_cdc_encoded() {
        let source = source_capabilities::POSTGRES;
        let sink = sink_capabilities::S3;
        let keys = vec!["id".to_string()];
        match derive_and_validate(&source, &sink, "users", &keys) {
            CompatibilityResult::Compatible(g) => {
                assert_eq!(g, EffectiveGuarantee::CdcEncoded);
            }
            CompatibilityResult::Incompatible(reasons) => {
                panic!("postgres->S3 should derive CdcEncoded, got: {:?}", reasons);
            }
        }
    }

    #[test]
    fn test_exact_once_pair_requires_business_keys() {
        let source = source_capabilities::POSTGRES;
        let sink = sink_capabilities::POSTGRES;
        let keys: Vec<String> = vec![];
        match derive_and_validate(&source, &sink, "users", &keys) {
            CompatibilityResult::Compatible(_) => {
                panic!("expected incompatible without business keys");
            }
            CompatibilityResult::Incompatible(reasons) => {
                assert!(reasons.iter().any(|r| r.contains("business_key_columns")));
            }
        }
    }

    #[test]
    fn test_cdc_encoded_pair_derives_cdc_encoded() {
        let source = source_capabilities::SQS;
        let sink = sink_capabilities::SNOWFLAKE;
        let keys: Vec<String> = vec![];
        match derive_and_validate(&source, &sink, "events", &keys) {
            CompatibilityResult::Compatible(g) => {
                assert_eq!(g, EffectiveGuarantee::CdcEncoded);
            }
            CompatibilityResult::Incompatible(reasons) => {
                panic!("expected cdc-encoded compatible, got: {:?}", reasons);
            }
        }
    }

    #[test]
    fn test_resolve_apply_action_upsert() {
        let action = resolve_apply_action(
            MutationKind::Insert,
            vec![("id".to_string(), "123".to_string())],
            vec![0, 0, 0, 1],
        );
        assert!(matches!(action, SinkApplyAction::UpsertIfNewer { .. }));
    }

    #[test]
    fn test_resolve_apply_action_delete() {
        let action = resolve_apply_action(
            MutationKind::Delete,
            vec![("id".to_string(), "123".to_string())],
            vec![0, 0, 0, 2],
        );
        assert!(matches!(action, SinkApplyAction::DeleteIfNewer { .. }));
    }

    #[test]
    fn test_resolve_apply_action_snapshot_is_upsert() {
        let action = resolve_apply_action(
            MutationKind::Snapshot,
            vec![("id".to_string(), "1".to_string())],
            vec![0],
        );
        assert!(matches!(action, SinkApplyAction::UpsertIfNewer { .. }));
    }

    #[test]
    fn test_wal_part_meta_serialization_roundtrip() {
        let meta = WalPartMeta {
            kind: WalPartKind::Cdc,
            row_count: 2,
            rows: vec![
                WalRowMeta {
                    mutation: MutationKind::Insert,
                    event_id: vec![1, 2, 3],
                    order_token: vec![0, 0, 0, 1],
                },
                WalRowMeta {
                    mutation: MutationKind::Delete,
                    event_id: vec![4, 5, 6],
                    order_token: vec![0, 0, 0, 2],
                },
            ],
        };
        let bytes = bincode::serialize(&meta).unwrap();
        let decoded: WalPartMeta = bincode::deserialize(&bytes).unwrap();
        assert_eq!(decoded.kind, WalPartKind::Cdc);
        assert_eq!(decoded.row_count, 2);
        assert_eq!(decoded.rows.len(), 2);
        assert_eq!(decoded.rows[0].mutation, MutationKind::Insert);
        assert_eq!(decoded.rows[1].mutation, MutationKind::Delete);
    }

    #[test]
    fn test_checkpoint_envelope_serialization_roundtrip() {
        let envelope = CheckpointEnvelope {
            authority: CheckpointAuthority::WalOwnership,
            kind: CheckpointKind::SourceResume,
            payload_version: 1,
            payload_bytes: vec![10, 20, 30],
        };
        let bytes = bincode::serialize(&envelope).unwrap();
        let decoded: CheckpointEnvelope = bincode::deserialize(&bytes).unwrap();
        assert_eq!(decoded.authority, CheckpointAuthority::WalOwnership);
        assert_eq!(decoded.kind, CheckpointKind::SourceResume);
        assert_eq!(decoded.payload_version, 1);
        assert_eq!(decoded.payload_bytes, vec![10, 20, 30]);
    }

    #[test]
    fn test_checkpoint_envelope_postgres_payload_roundtrip() {
        let payload = PostgresCheckpoint {
            lsn: 0x1234_5678_9abc_def0,
            slot_name: "skippr_slot".to_string(),
        };
        let envelope = CheckpointEnvelope::from_payload(
            CheckpointAuthority::WalOwnership,
            CheckpointKind::BootstrapAnchor,
            1,
            &payload,
        )
        .unwrap();
        let decoded: PostgresCheckpoint = envelope.into_payload().unwrap();
        assert_eq!(decoded.lsn, payload.lsn);
        assert_eq!(decoded.slot_name, payload.slot_name);
    }

    #[test]
    fn test_checkpoint_envelope_kafka_payload_roundtrip() {
        let payload = KafkaCheckpoint {
            topic: "events".to_string(),
            partition: 3,
            offset: 42,
        };
        let envelope = CheckpointEnvelope::from_payload(
            CheckpointAuthority::WalOwnership,
            CheckpointKind::SourceResume,
            1,
            &payload,
        )
        .unwrap();
        let decoded: KafkaCheckpoint = envelope.into_payload().unwrap();
        assert_eq!(decoded.topic, payload.topic);
        assert_eq!(decoded.partition, payload.partition);
        assert_eq!(decoded.offset, payload.offset);
    }

    #[test]
    fn test_checkpoint_envelope_into_payload_wrong_type_errors() {
        let payload = PostgresCheckpoint {
            lsn: 1,
            slot_name: "s".to_string(),
        };
        let envelope = CheckpointEnvelope::from_payload(
            CheckpointAuthority::WalOwnership,
            CheckpointKind::BootstrapAnchor,
            1,
            &payload,
        )
        .unwrap();
        let err = envelope.into_payload::<KafkaCheckpoint>();
        assert!(err.is_err());
    }
}
