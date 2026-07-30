use std::collections::BTreeMap;

use crate::buffer::compaction_transaction::{
    SinkGroupingSupport, SinkRetrySemantics, SinkWriteSemantics,
};
// `skippr-runtime-sdk` re-exports this module because the current dependency
// graph is `skippr-runtime-sdk -> skippr-core -> skippr`.
use crate::discover::OutputMetadata;
use crate::helpers::configuration::{DataSinkPluginConfig, DataSourcePluginConfig};
use crate::helpers::offsets::{OffsetKey, RuntimeOffsetRpcRequest, RuntimeOffsetRpcResponse};
use crate::plugins::cdc::{
    CheckpointEnvelope, EventIdSemantics, SinkCapability, SinkGuaranteeTier, SourceBootstrapStyle,
    SourceCapability, SourceCheckpointStyle, SourceGuaranteeTier, SourceOrderModel, SyncContext,
};
use crate::plugins::source_contract::{SinkWritePolicySupport, SourceNamespaceContract};
use crate::plugins::SinkWriteOutcome;
use crate::sink_apply_identity::SinkApplyEnvelopeV2;
use serde::{Deserialize, Serialize};

// This is the in-process runtime plugin IPC contract. It is deliberately
// separate from the skippr/React adapter's CLI subprocess JSON summaries.
// Schema freshness is negotiated through required_schema_version plus
// SchemaStateRefreshRequired, not by sending discover stdout metadata payloads.
pub const RUNTIME_PROTOCOL_VERSION: u32 = 17;
pub const COMMIT_RECEIPT_VERSION: u32 = 1;
pub const CATALOG_INTENT_VERSION: u32 = 1;
pub const MAX_RUNTIME_SINK_CHUNK_BYTES: usize = 128 * 1024 * 1024;
pub const SKIPPR_RUNTIME_CONTROL_ADDR_ENV: &str = "SKIPPR_RUNTIME_CONTROL_ADDR";
pub const SKIPPR_RUNTIME_DATA_ADDR_ENV: &str = "SKIPPR_RUNTIME_DATA_ADDR";
pub const SKIPPR_RUNTIME_OFFSET_ADDR_ENV: &str = "SKIPPR_RUNTIME_OFFSET_ADDR";
pub const SKIPPR_RUNTIME_SESSION_TOKEN_ENV: &str = "SKIPPR_RUNTIME_SESSION_TOKEN";
/// Set on runtime source children by the host (`discover` | `sync`). Not a customer-facing setting.
pub const SKIPPR_RUNTIME_EXECUTION_MODE_ENV: &str = "SKIPPR_RUNTIME_EXECUTION_MODE";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RuntimeSessionHello {
    pub protocol_version: u32,
    pub token: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum RuntimePluginKind {
    DataSource,
    DataSink,
    SchemaSink,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum RuntimeBinding {
    Primary,
    Deadletter,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RuntimeSourceCapabilityDescriptor {
    pub name: String,
    pub guarantee_tier: SourceGuaranteeTier,
    pub checkpoint_style: SourceCheckpointStyle,
    pub bootstrap_style: SourceBootstrapStyle,
    pub order_model: SourceOrderModel,
    pub supports_deletes: bool,
    pub event_id_semantics: EventIdSemantics,
    #[serde(default)]
    pub declares_namespace_contracts: bool,
}

impl From<&'static SourceCapability> for RuntimeSourceCapabilityDescriptor {
    fn from(value: &'static SourceCapability) -> Self {
        Self {
            name: value.name.to_string(),
            guarantee_tier: value.guarantee_tier,
            checkpoint_style: value.checkpoint_style,
            bootstrap_style: value.bootstrap_style,
            order_model: value.order_model,
            supports_deletes: value.supports_deletes,
            event_id_semantics: value.event_id_semantics,
            declares_namespace_contracts: false,
        }
    }
}

impl RuntimeSourceCapabilityDescriptor {
    pub fn to_cdc_capability(&self) -> SourceCapability {
        SourceCapability {
            name: Box::leak(self.name.clone().into_boxed_str()),
            guarantee_tier: self.guarantee_tier,
            checkpoint_style: self.checkpoint_style,
            bootstrap_style: self.bootstrap_style,
            order_model: self.order_model,
            supports_deletes: self.supports_deletes,
            event_id_semantics: self.event_id_semantics,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RuntimeSinkCapabilityDescriptor {
    pub name: String,
    pub guarantee_tier: SinkGuaranteeTier,
    pub can_manage_skippr_columns: bool,
    pub can_maintain_tombstone_tables: bool,
    pub can_compare_order_tokens: bool,
    pub supports_transactions: bool,
    #[serde(default)]
    pub supports_merge_by_key: bool,
    #[serde(default)]
    pub supports_replace_table: bool,
    #[serde(default)]
    pub supports_replace_partition: bool,
    #[serde(default)]
    pub supports_primary_key_metadata: bool,
    #[serde(default)]
    pub supports_bounded_grouped_stream: bool,
    pub retry_semantics: SinkRetrySemantics,
    pub grouping_support: SinkGroupingSupport,
}

impl From<&SinkCapability> for RuntimeSinkCapabilityDescriptor {
    fn from(value: &SinkCapability) -> Self {
        let flags = Self::default_write_policy_flags_for_sink(value.name);
        Self {
            name: value.name.to_string(),
            guarantee_tier: value.guarantee_tier,
            can_manage_skippr_columns: value.can_manage_skippr_columns,
            can_maintain_tombstone_tables: value.can_maintain_tombstone_tables,
            can_compare_order_tokens: value.can_compare_order_tokens,
            supports_transactions: value.supports_transactions,
            supports_merge_by_key: flags.supports_merge_by_key,
            supports_replace_table: flags.supports_replace_table,
            supports_replace_partition: flags.supports_replace_partition,
            supports_primary_key_metadata: Self::supports_primary_key_metadata_for_sink(value.name),
            supports_bounded_grouped_stream: !value.grouping_support.is_none(),
            retry_semantics: value.retry_semantics,
            grouping_support: value.grouping_support,
        }
    }
}

impl RuntimeSinkCapabilityDescriptor {
    fn supports_primary_key_metadata_for_sink(name: &str) -> bool {
        matches!(name, "Athena")
    }

    fn default_write_policy_flags_for_sink(name: &str) -> SinkWritePolicySupport {
        let mut support = SinkWritePolicySupport::default();
        match name {
            "Athena" => {
                support.supports_replace_partition = true;
                support.supports_replace_table = true;
            }
            "Iceberg" => {
                support.supports_merge_by_key = true;
                support.supports_replace_partition = true;
                support.supports_replace_table = true;
            }
            _ => {}
        }
        support
    }
}

impl RuntimeSinkCapabilityDescriptor {
    pub fn to_cdc_capability(&self) -> SinkCapability {
        SinkCapability {
            name: Box::leak(self.name.clone().into_boxed_str()),
            guarantee_tier: self.guarantee_tier,
            can_manage_skippr_columns: self.can_manage_skippr_columns,
            can_maintain_tombstone_tables: self.can_maintain_tombstone_tables,
            can_compare_order_tokens: self.can_compare_order_tokens,
            supports_transactions: self.supports_transactions,
            retry_semantics: self.retry_semantics,
            grouping_support: self.grouping_support,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimePluginConfigEnvelope {
    pub plugin_name: String,
    pub raw_config_json: String,
}

impl From<DataSourcePluginConfig> for RuntimePluginConfigEnvelope {
    fn from(value: DataSourcePluginConfig) -> Self {
        Self::new(value.plugin_name, value.config)
    }
}

impl RuntimePluginConfigEnvelope {
    pub fn new(plugin_name: impl Into<String>, raw_config: serde_json::Value) -> Self {
        Self {
            plugin_name: plugin_name.into(),
            raw_config_json: raw_config.to_string(),
        }
    }

    pub fn expect_plugin(&self, expected_plugin_name: &str) -> Result<(), String> {
        if self.plugin_name == expected_plugin_name {
            Ok(())
        } else {
            Err(format!(
                "runtime config targets plugin '{}' but '{}' was expected",
                self.plugin_name, expected_plugin_name
            ))
        }
    }

    pub fn decode<T: serde::de::DeserializeOwned>(&self) -> Result<T, String> {
        serde_json::from_str(&self.raw_config_json).map_err(|err| {
            format!(
                "Failed to decode runtime config for '{}': {}",
                self.plugin_name, err
            )
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeSourceConfig(pub RuntimePluginConfigEnvelope);

impl TryFrom<DataSourcePluginConfig> for RuntimeSourceConfig {
    type Error = String;

    fn try_from(value: DataSourcePluginConfig) -> Result<Self, Self::Error> {
        Ok(Self(value.into()))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeSinkConfig(pub RuntimePluginConfigEnvelope);

impl TryFrom<DataSinkPluginConfig> for RuntimeSinkConfig {
    type Error = String;

    fn try_from(value: DataSinkPluginConfig) -> Result<Self, Self::Error> {
        Ok(Self(value.into()))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeSchemaConfig(pub RuntimePluginConfigEnvelope);

impl TryFrom<DataSinkPluginConfig> for RuntimeSchemaConfig {
    type Error = String;

    fn try_from(value: DataSinkPluginConfig) -> Result<Self, Self::Error> {
        Ok(Self(value.into()))
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct RuntimeOutputLayout {
    pub partition_fields: Vec<String>,
    #[serde(default)]
    pub order_fields: Vec<String>,
    pub time_partition_granularity: Option<String>,
    #[serde(default)]
    pub time_partition_prefix: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub enum RuntimeExecutionMode {
    Discover,
    #[default]
    Sync,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RuntimeExecutionContext {
    pub pipeline_name: String,
    pub workspace_name: String,
    pub data_dir: String,
    #[serde(default)]
    pub execution_mode: RuntimeExecutionMode,
    #[serde(default)]
    pub output_layout: RuntimeOutputLayout,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct RuntimeSchemaState {
    pub version: u64,
    pub namespaces: BTreeMap<String, OutputMetadata>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct SchemaNamespaceDelta {
    pub version: u64,
    pub metadata: OutputMetadata,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct SchemaDelta {
    pub version: u64,
    pub namespaces: BTreeMap<String, SchemaNamespaceDelta>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeSinkInstallRequest {
    pub context: RuntimeExecutionContext,
    pub binding: RuntimeBinding,
    pub config: RuntimeSinkConfig,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeSchemaInstallRequest {
    pub context: RuntimeExecutionContext,
    pub binding: RuntimeBinding,
    pub config: RuntimeSchemaConfig,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct RuntimeSchemaStateInstallRequest {
    pub schema_state: RuntimeSchemaState,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RuntimeSchemaRefreshRequest {
    pub request_id: u64,
    pub required_version: u64,
    pub installed_version: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct HandshakeRequest {
    pub pipeline_name: String,
    pub protocol_version: u32,
    /// Maximum concurrent sink sessions the host will route through this child.
    /// Non-sink runtimes receive one.
    pub sink_session_capacity: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct HandshakeResponse {
    pub protocol_version: u32,
    pub kind: RuntimePluginKind,
    pub plugin_name: String,
    pub source_capability: Option<RuntimeSourceCapabilityDescriptor>,
    pub sink_capability: Option<RuntimeSinkCapabilityDescriptor>,
    pub supports_schema: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeSourceIngestWindow {
    pub max_in_flight_requests: usize,
    pub max_in_flight_bytes: usize,
}

impl Default for RuntimeSourceIngestWindow {
    fn default() -> Self {
        Self {
            max_in_flight_requests: 8,
            max_in_flight_bytes: 512 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SourceStartRequest {
    pub context: RuntimeExecutionContext,
    pub config: RuntimeSourceConfig,
    // Keep this trailing so v7 runtimes can decode RunSource and ignore it.
    #[serde(default)]
    pub once: bool,
    #[serde(default)]
    pub source_ingest_window: RuntimeSourceIngestWindow,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeCheckpointUpdate {
    pub key: String,
    pub envelope: CheckpointEnvelope,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RuntimeOffsetPosition {
    pub key: OffsetKey,
    pub position: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RuntimeOffsetMaterializationHint {
    pub key: OffsetKey,
    pub position: u64,
    pub closed: bool,
}

/// Generic offset validation entry for batched source offset checks.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RuntimeOffsetValidationEntry {
    pub key: OffsetKey,
    pub offset_type: crate::helpers::offsets::OffsetTypes,
    pub offset_value: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum PluginOffsetFrame {
    ValidateOffsetBatch {
        request_id: u64,
        entries: Vec<RuntimeOffsetValidationEntry>,
    },
    LoadCheckpoint {
        request_id: u64,
        key: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum HostOffsetFrame {
    ValidateOffsetBatchResponse {
        request_id: u64,
        should_process: Vec<bool>,
    },
    LoadCheckpointResponse {
        request_id: u64,
        envelope: Option<CheckpointEnvelope>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeIngestPartitionBatch {
    pub sink_ref: String,
    pub namespace: String,
    pub partition: String,
    pub time: Option<i64>,
    pub schema_fingerprint: String,
    pub offsets: Vec<RuntimeOffsetPosition>,
    pub arrow_stream_bytes: Vec<u8>,
    pub cdc_rows: Option<Vec<crate::plugins::cdc::WalRowMeta>>,
    /// When set, persisted atomically with this batch's WAL segment after offsets are durable.
    #[serde(default)]
    pub checkpoint_update: Option<RuntimeCheckpointUpdate>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeRawIngestBatch {
    pub offset_key: OffsetKey,
    pub data: String,
    pub bytes: usize,
    /// Always present on the bincode wire (use `None` when unset). Do not use
    /// `skip_serializing_if` here — bincode + serde will not apply defaults for omitted fields.
    #[serde(default)]
    pub offset_pos: Option<u64>,
    pub source_uri: String,
    pub namespace: Option<String>,
    pub cdc_rows: Option<Vec<crate::plugins::cdc::WalRowMeta>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeSourceSinkWrite {
    pub filename: String,
    pub compaction_id: String,
    pub idempotency_key: String,
    pub wal_refs: Vec<RuntimeWalPartRef>,
    pub write_semantics: SinkWriteSemantics,
    pub schema_fingerprint: String,
    pub arrow_stream_bytes: Vec<u8>,
    pub cdc_ctx: Option<SyncContext>,
    /// Always present on the bincode wire (use `None` when unset). Do not use
    /// `skip_serializing_if` here — bincode + serde will not apply defaults for omitted fields.
    #[serde(default)]
    pub source_contract: Option<SourceNamespaceContract>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RuntimeWalPartRef {
    pub segment_id: String,
    pub source: String,
    pub start: u64,
    pub len: u64,
    pub sink_ref: String,
    pub namespace: String,
    pub partition: String,
    pub time: Option<i64>,
    pub schema_fingerprint: String,
    pub cdc_meta_hash: Option<[u8; 32]>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RuntimeRequestAck {
    pub request_id: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum CommitReceiptAuthority {
    SinkWrite,
    AuthoritativePreflight { authority: String },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CommitReceipt {
    pub version: u32,
    pub compaction_id: String,
    pub idempotency_key: String,
    pub wal_refs_fingerprint: String,
    pub authority: CommitReceiptAuthority,
}

impl CommitReceipt {
    pub fn from_envelope(
        envelope: &SinkApplyEnvelopeV2,
        authority: CommitReceiptAuthority,
    ) -> Self {
        Self {
            version: COMMIT_RECEIPT_VERSION,
            compaction_id: envelope.compaction_id.clone(),
            idempotency_key: envelope.idempotency_key.clone(),
            wal_refs_fingerprint: envelope.wal_refs_fingerprint.clone(),
            authority,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct SinkWriteStats {
    pub rows: Option<u64>,
    pub bytes: Option<u64>,
    pub objects: Option<u64>,
    pub encode_duration_ms: Option<u64>,
    pub upload_duration_ms: Option<u64>,
    pub commit_duration_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub enum CatalogIntentKind {
    UpsertNamespace,
    UpsertPartition,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct CatalogIntentIdentity {
    pub sink_ref: String,
    pub namespace: String,
    pub kind: CatalogIntentKind,
    pub key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct CatalogIntent {
    pub version: u32,
    pub identity: CatalogIntentIdentity,
    pub payload_json: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SinkAck {
    pub request_id: u64,
    pub outcome: SinkWriteOutcome,
    pub receipt: CommitReceipt,
    pub stats: SinkWriteStats,
    pub catalog_intents: Vec<CatalogIntent>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RuntimeIngestAck {
    pub request_id: u64,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum SourceEvent {
    SchemaStateUpdate(RuntimeSchemaState),
    SchemaDelta(SchemaDelta),
    ContractsUpdate(Vec<SourceNamespaceContract>),
    Completed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SinkRunRequest {
    pub request_id: u64,
    pub compaction_id: String,
    pub idempotency_key: String,
    pub wal_refs: Vec<RuntimeWalPartRef>,
    pub write_semantics: SinkWriteSemantics,
    pub schema_fingerprint: String,
    pub binding: RuntimeBinding,
    pub required_schema_version: u64,
    pub filename: String,
    pub cdc_ctx: Option<SyncContext>,
    /// Always present on the bincode wire (use `None` when unset). Do not use
    /// `skip_serializing_if` here — bincode + serde will not apply defaults for omitted fields.
    #[serde(default)]
    pub source_contract: Option<SourceNamespaceContract>,
    #[serde(default)]
    pub payload_mode: RuntimeSinkPayloadMode,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PrepareSink {
    pub request_id: u64,
    pub envelope: SinkApplyEnvelopeV2,
    pub request: SinkRunRequest,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum PrepareSinkResult {
    Ready,
    AlreadyApplied(CommitReceipt),
    Rejected { reason: String },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PrepareAck {
    pub request_id: u64,
    pub result: PrepareSinkResult,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RuntimeSinkError {
    pub request_id: u64,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub enum RuntimeSinkPayloadMode {
    #[default]
    FullStream,
    GroupedChunks,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SinkChunk {
    pub request_id: u64,
    pub chunk_index: u32,
    pub row_offset: u64,
    pub rows: u64,
    pub arrow_stream_bytes: Vec<u8>,
}

impl SinkChunk {
    pub fn validate_bound(&self) -> Result<(), String> {
        if self.arrow_stream_bytes.len() > MAX_RUNTIME_SINK_CHUNK_BYTES {
            return Err(format!(
                "sink chunk {} for request {} is {} bytes; maximum is {}",
                self.chunk_index,
                self.request_id,
                self.arrow_stream_bytes.len(),
                MAX_RUNTIME_SINK_CHUNK_BYTES
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct FinishSink {
    pub request_id: u64,
    pub chunks: u32,
    pub rows: u64,
    pub bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SchemaRunRequest {
    pub request_id: u64,
    pub compaction_id: String,
    pub binding: RuntimeBinding,
    pub required_schema_version: u64,
    pub namespace: String,
    #[serde(default)]
    pub source_contract: Option<SourceNamespaceContract>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum HostFrame {
    Handshake(HandshakeRequest),
    InstallSink(RuntimeSinkInstallRequest),
    InstallSchema(RuntimeSchemaInstallRequest),
    InstallSchemaState(RuntimeSchemaStateInstallRequest),
    InstallSchemaDelta(SchemaDelta),
    RunSource(SourceStartRequest),
    PrepareSink(PrepareSink),
    RunSchema(SchemaRunRequest),
    IngestAck(RuntimeIngestAck),
    OffsetResponse(RuntimeOffsetRpcResponse),
    Shutdown,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum PluginFrame {
    HandshakeAck(HandshakeResponse),
    Installed,
    SourceEvent(SourceEvent),
    OffsetRequest(RuntimeOffsetRpcRequest),
    PrepareAck(PrepareAck),
    SinkAck(SinkAck),
    SinkError(RuntimeSinkError),
    SchemaAck(RuntimeRequestAck),
    SchemaStateRefreshRequired(RuntimeSchemaRefreshRequest),
    Error(String),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum HostDataFrame {
    SinkChunk(SinkChunk),
    FinishSink(FinishSink),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum PluginDataFrame {
    /// Host-owned ingest payloads emitted by runtime source plugins.
    SourcePayloadBatches {
        request_id: u64,
        tasks: Vec<Vec<RuntimeRawIngestBatch>>,
    },
    IngestBatches {
        request_id: u64,
        batches: Vec<RuntimeIngestPartitionBatch>,
    },
    CheckpointUpdate {
        update: RuntimeCheckpointUpdate,
    },
    OffsetMaterializationHints {
        hints: Vec<RuntimeOffsetMaterializationHint>,
    },
    SinkWrite(RuntimeSourceSinkWrite),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_start_roundtrips_at_v17() {
        let frame = HostFrame::RunSource(SourceStartRequest {
            context: RuntimeExecutionContext {
                pipeline_name: "pipeline".to_string(),
                workspace_name: "workspace".to_string(),
                data_dir: "/tmp/data".to_string(),
                execution_mode: RuntimeExecutionMode::Sync,
                output_layout: RuntimeOutputLayout::default(),
            },
            config: RuntimeSourceConfig(RuntimePluginConfigEnvelope::new(
                "Test",
                serde_json::json!({}),
            )),
            once: true,
            source_ingest_window: RuntimeSourceIngestWindow::default(),
        });

        let bytes = bincode::serialize(&frame).unwrap();
        let decoded: HostFrame = bincode::deserialize(&bytes).unwrap();

        let HostFrame::RunSource(decoded) = decoded else {
            panic!("expected RunSource frame");
        };
        assert_eq!(decoded.context.pipeline_name, "pipeline");
        assert_eq!(decoded.context.execution_mode, RuntimeExecutionMode::Sync);
        assert!(decoded.once);
    }

    #[test]
    fn source_event_contracts_update_roundtrips() {
        use crate::plugins::source_contract::{FieldPath, SourceNamespaceContract, WritePolicy};

        let event = SourceEvent::ContractsUpdate(vec![SourceNamespaceContract {
            namespace: "google_analytics.events_daily".into(),
            primary_key: vec![FieldPath::single("property_id"), FieldPath::single("date")],
            cursor: Some(FieldPath::single("date")),
            partition_key: vec![FieldPath::single("date")],
            write_policy: WritePolicy::ReplacePartition,
            refresh_window: Some(3),
            description: String::new(),
            semantics: None,
        }]);
        let frame = PluginFrame::SourceEvent(event);
        let bytes = bincode::serialize(&frame).unwrap();
        let decoded: PluginFrame = bincode::deserialize(&bytes).unwrap();
        match decoded {
            PluginFrame::SourceEvent(SourceEvent::ContractsUpdate(contracts)) => {
                assert_eq!(contracts.len(), 1);
                assert_eq!(contracts[0].write_policy, WritePolicy::ReplacePartition);
            }
            other => panic!("unexpected frame: {:?}", other),
        }
    }

    #[test]
    fn sink_run_request_optional_source_contract_defaults_none() {
        let bytes = bincode::serialize(&SinkRunRequest {
            request_id: 1,
            compaction_id: "c1".into(),
            idempotency_key: "c1".into(),
            wal_refs: Vec::new(),
            write_semantics: SinkWriteSemantics::AtLeastOnce,
            schema_fingerprint: String::new(),
            binding: RuntimeBinding::Primary,
            required_schema_version: 0,
            filename: "f".into(),
            cdc_ctx: None,
            source_contract: None,
            payload_mode: RuntimeSinkPayloadMode::FullStream,
        })
        .unwrap();
        let decoded: SinkRunRequest = bincode::deserialize(&bytes).unwrap();
        assert!(decoded.source_contract.is_none());
    }

    #[test]
    fn source_payload_batch_none_offset_pos_roundtrips() {
        use crate::helpers::offsets::OffsetKey;

        let frame = PluginDataFrame::SourcePayloadBatches {
            request_id: 42,
            tasks: vec![vec![RuntimeRawIngestBatch {
                offset_key: OffsetKey::new("orders", "partition-1"),
                data: "{}\n".into(),
                bytes: 3,
                offset_pos: None,
                source_uri: "mysql://localhost/orders".into(),
                namespace: Some("mysql.orders".into()),
                cdc_rows: None,
            }]],
        };

        let bytes = bincode::serialize(&frame).unwrap();
        let decoded: PluginDataFrame = bincode::deserialize(&bytes).unwrap();

        let PluginDataFrame::SourcePayloadBatches { request_id, tasks } = decoded else {
            panic!("expected SourcePayloadBatches frame");
        };
        assert_eq!(request_id, 42);
        assert_eq!(tasks[0][0].offset_pos, None);
        assert_eq!(tasks[0][0].source_uri, "mysql://localhost/orders");
    }

    #[test]
    fn sink_write_none_source_contract_roundtrips() {
        let frame = PluginDataFrame::SinkWrite(RuntimeSourceSinkWrite {
            filename: "orders/part-0001.arrow".into(),
            compaction_id: "c1".into(),
            idempotency_key: "c1".into(),
            wal_refs: Vec::new(),
            write_semantics: SinkWriteSemantics::AtLeastOnce,
            schema_fingerprint: String::new(),
            arrow_stream_bytes: vec![1, 2, 3],
            cdc_ctx: None,
            source_contract: None,
        });

        let bytes = bincode::serialize(&frame).unwrap();
        let decoded: PluginDataFrame = bincode::deserialize(&bytes).unwrap();

        let PluginDataFrame::SinkWrite(decoded) = decoded else {
            panic!("expected SinkWrite frame");
        };
        assert!(decoded.source_contract.is_none());
        assert_eq!(decoded.filename, "orders/part-0001.arrow");
    }

    #[test]
    fn sink_chunk_and_finish_roundtrip() {
        let frame = HostDataFrame::SinkChunk(SinkChunk {
            request_id: 7,
            chunk_index: 1,
            row_offset: 10,
            rows: 5,
            arrow_stream_bytes: vec![1, 2, 3],
        });
        let bytes = bincode::serialize(&frame).unwrap();
        let decoded: HostDataFrame = bincode::deserialize(&bytes).unwrap();
        match decoded {
            HostDataFrame::SinkChunk(chunk) => {
                assert_eq!(chunk.request_id, 7);
                assert_eq!(chunk.chunk_index, 1);
                assert_eq!(chunk.row_offset, 10);
                assert_eq!(chunk.rows, 5);
                assert_eq!(chunk.arrow_stream_bytes, vec![1, 2, 3]);
            }
            HostDataFrame::FinishSink(_) => panic!("expected chunk"),
        }

        let finish = HostDataFrame::FinishSink(FinishSink {
            request_id: 7,
            chunks: 2,
            rows: 5,
            bytes: 3,
        });
        let decoded: HostDataFrame =
            bincode::deserialize(&bincode::serialize(&finish).unwrap()).unwrap();
        assert!(matches!(
            decoded,
            HostDataFrame::FinishSink(FinishSink { chunks: 2, .. })
        ));
    }

    #[test]
    fn sink_run_request_source_contract_roundtrips() {
        use crate::plugins::source_contract::{FieldPath, SourceNamespaceContract, WritePolicy};

        let contract = SourceNamespaceContract {
            namespace: "google_analytics.events_daily".into(),
            primary_key: vec![FieldPath::single("date")],
            cursor: Some(FieldPath::single("date")),
            partition_key: vec![FieldPath::single("date")],
            write_policy: WritePolicy::ReplacePartition,
            refresh_window: Some(3),
            description: String::new(),
            semantics: None,
        };
        let bytes = bincode::serialize(&SinkRunRequest {
            request_id: 2,
            compaction_id: "c2".into(),
            idempotency_key: "c2".into(),
            wal_refs: Vec::new(),
            write_semantics: SinkWriteSemantics::AtLeastOnce,
            schema_fingerprint: String::new(),
            binding: RuntimeBinding::Primary,
            required_schema_version: 1,
            filename: "ns/part.parquet".into(),
            cdc_ctx: None,
            source_contract: Some(contract.clone()),
            payload_mode: RuntimeSinkPayloadMode::FullStream,
        })
        .unwrap();
        let decoded: SinkRunRequest = bincode::deserialize(&bytes).unwrap();
        let roundtrip = decoded.source_contract.expect("contract");
        assert_eq!(roundtrip.namespace, contract.namespace);
        assert_eq!(roundtrip.write_policy, WritePolicy::ReplacePartition);
    }

    #[test]
    fn schema_run_request_source_contract_roundtrips() {
        use crate::plugins::source_contract::{FieldPath, SourceNamespaceContract, WritePolicy};

        let contract = SourceNamespaceContract {
            namespace: "google_analytics.events_daily".into(),
            primary_key: vec![FieldPath::single("date")],
            cursor: None,
            partition_key: vec![FieldPath::single("date")],
            write_policy: WritePolicy::ReplacePartition,
            refresh_window: None,
            description: String::new(),
            semantics: None,
        };
        let request = SchemaRunRequest {
            request_id: 3,
            compaction_id: "schema-c1".into(),
            binding: RuntimeBinding::Primary,
            required_schema_version: 2,
            namespace: "google_analytics.events_daily".into(),
            source_contract: Some(contract),
        };
        let bytes = bincode::serialize(&request).unwrap();
        let decoded: SchemaRunRequest = bincode::deserialize(&bytes).unwrap();
        assert_eq!(
            decoded
                .source_contract
                .as_ref()
                .map(|c| c.partition_key[0].dotted()),
            Some("date".to_string())
        );
    }

    #[test]
    fn contracts_update_empty_clears_via_roundtrip() {
        let event = SourceEvent::ContractsUpdate(vec![]);
        let frame = PluginFrame::SourceEvent(event);
        let bytes = bincode::serialize(&frame).unwrap();
        let decoded: PluginFrame = bincode::deserialize(&bytes).unwrap();
        match decoded {
            PluginFrame::SourceEvent(SourceEvent::ContractsUpdate(contracts)) => {
                assert!(contracts.is_empty());
            }
            other => panic!("unexpected frame: {:?}", other),
        }
    }

    #[test]
    fn sink_ack_stats_roundtrip_without_fake_unknowns() {
        let receipt = CommitReceipt {
            version: COMMIT_RECEIPT_VERSION,
            compaction_id: "c1".into(),
            idempotency_key: "k1".into(),
            wal_refs_fingerprint: "refs".into(),
            authority: CommitReceiptAuthority::SinkWrite,
        };
        let frame = PluginFrame::SinkAck(SinkAck {
            request_id: 11,
            outcome: SinkWriteOutcome::Applied,
            receipt: receipt.clone(),
            stats: SinkWriteStats {
                rows: Some(42),
                bytes: Some(1024),
                objects: None,
                encode_duration_ms: None,
                upload_duration_ms: Some(9),
                commit_duration_ms: None,
            },
            catalog_intents: Vec::new(),
        });
        let decoded: PluginFrame =
            bincode::deserialize(&bincode::serialize(&frame).unwrap()).unwrap();
        let PluginFrame::SinkAck(decoded) = decoded else {
            panic!("expected SinkAck");
        };
        assert_eq!(decoded.receipt, receipt);
        assert_eq!(decoded.stats.rows, Some(42));
        assert_eq!(decoded.stats.objects, None);
        assert_eq!(decoded.stats.commit_duration_ms, None);
    }

    #[test]
    fn catalog_intent_identity_is_serializable_and_deduplicable() {
        use std::collections::BTreeSet;

        let intent = CatalogIntent {
            version: CATALOG_INTENT_VERSION,
            identity: CatalogIntentIdentity {
                sink_ref: "primary".into(),
                namespace: "events".into(),
                kind: CatalogIntentKind::UpsertPartition,
                key: "day=2026-07-30".into(),
            },
            payload_json: r#"{"location":"s3://bucket/events/day=2026-07-30"}"#.into(),
        };
        let decoded: CatalogIntent =
            bincode::deserialize(&bincode::serialize(&intent).unwrap()).unwrap();
        assert_eq!(decoded, intent);
        assert_eq!(
            BTreeSet::from([intent.clone(), decoded])
                .into_iter()
                .count(),
            1
        );
    }

    #[test]
    fn schema_delta_roundtrips_changed_namespace_versions() {
        let delta = SchemaDelta {
            version: 9,
            namespaces: BTreeMap::from([(
                "events".into(),
                SchemaNamespaceDelta {
                    version: 9,
                    metadata: OutputMetadata::new(),
                },
            )]),
        };
        let frame = HostFrame::InstallSchemaDelta(delta.clone());
        let decoded: HostFrame =
            bincode::deserialize(&bincode::serialize(&frame).unwrap()).unwrap();
        let HostFrame::InstallSchemaDelta(decoded) = decoded else {
            panic!("expected SchemaDelta");
        };
        assert_eq!(decoded, delta);
        assert_eq!(decoded.namespaces["events"].version, 9);
    }

    #[test]
    fn protocol_version_is_hard_cut_to_v17() {
        assert_eq!(RUNTIME_PROTOCOL_VERSION, 17);
    }

    #[test]
    fn athena_sink_capability_handshake_matches_plugin_manifest_metadata() {
        use crate::buffer::compaction_transaction::{SinkGroupingSupport, SinkRetrySemantics};
        use crate::plugins::cdc::{sink_capabilities, SinkGuaranteeTier};

        let handshake = RuntimeSinkCapabilityDescriptor::from(&sink_capabilities::ATHENA);
        let manifest = RuntimeSinkCapabilityDescriptor {
            name: "Athena".into(),
            guarantee_tier: SinkGuaranteeTier::CdcEncodedOnly,
            can_manage_skippr_columns: false,
            can_maintain_tombstone_tables: false,
            can_compare_order_tokens: false,
            supports_transactions: false,
            supports_merge_by_key: false,
            supports_replace_table: true,
            supports_replace_partition: true,
            supports_primary_key_metadata: true,
            supports_bounded_grouped_stream: true,
            retry_semantics: SinkRetrySemantics::DeterministicOverwrite,
            grouping_support: SinkGroupingSupport::CdcEncodedBatches,
        };
        assert_eq!(handshake, manifest);
    }
}
