use std::collections::BTreeMap;

// `skippr-runtime-sdk` re-exports this module because the current dependency
// graph is `skippr-runtime-sdk -> skippr-core -> skippr`.
use crate::discover::OutputMetadata;
use crate::helpers::configuration::{DataSinkPluginConfig, DataSourcePluginConfig};
use crate::helpers::offsets::{OffsetKey, RuntimeOffsetRpcRequest, RuntimeOffsetRpcResponse};
use crate::plugins::cdc::{
    CheckpointEnvelope, EventIdSemantics, SinkCapability, SinkGuaranteeTier, SourceBootstrapStyle,
    SourceCapability, SourceCheckpointStyle, SourceGuaranteeTier, SourceOrderModel, SyncContext,
};
use serde::{Deserialize, Serialize};

pub const RUNTIME_PROTOCOL_VERSION: u32 = 6;
pub const SKIPPR_RUNTIME_CONTROL_ADDR_ENV: &str = "SKIPPR_RUNTIME_CONTROL_ADDR";
pub const SKIPPR_RUNTIME_DATA_ADDR_ENV: &str = "SKIPPR_RUNTIME_DATA_ADDR";
pub const SKIPPR_RUNTIME_SESSION_TOKEN_ENV: &str = "SKIPPR_RUNTIME_SESSION_TOKEN";

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
}

impl From<&'static SinkCapability> for RuntimeSinkCapabilityDescriptor {
    fn from(value: &'static SinkCapability) -> Self {
        Self {
            name: value.name.to_string(),
            guarantee_tier: value.guarantee_tier,
            can_manage_skippr_columns: value.can_manage_skippr_columns,
            can_maintain_tombstone_tables: value.can_maintain_tombstone_tables,
            can_compare_order_tokens: value.can_compare_order_tokens,
            supports_transactions: value.supports_transactions,
        }
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
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RuntimeExecutionContext {
    pub pipeline_name: String,
    pub workspace_name: String,
    pub data_dir: String,
    #[serde(default)]
    pub output_layout: RuntimeOutputLayout,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct RuntimeSchemaState {
    pub version: u64,
    pub namespaces: BTreeMap<String, OutputMetadata>,
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
    pub required_version: u64,
    pub installed_version: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct HandshakeRequest {
    pub pipeline_name: String,
    pub protocol_version: u32,
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
pub struct SourceStartRequest {
    pub context: RuntimeExecutionContext,
    pub config: RuntimeSourceConfig,
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

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeIngestPartitionBatch {
    pub sink_ref: String,
    pub namespace: String,
    pub partition: String,
    pub time: Option<i64>,
    pub shard: String,
    pub offsets: Vec<RuntimeOffsetPosition>,
    pub arrow_stream_bytes: Vec<u8>,
    pub cdc_rows: Option<Vec<crate::plugins::cdc::WalRowMeta>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeSourceSinkWrite {
    pub filename: String,
    pub compaction_id: String,
    pub arrow_stream_bytes: Vec<u8>,
    pub cdc_ctx: Option<SyncContext>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RuntimeRequestAck {
    pub request_id: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum SourceEvent {
    SchemaStateUpdate(RuntimeSchemaState),
    Completed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SinkRunRequest {
    pub request_id: u64,
    pub compaction_id: String,
    pub binding: RuntimeBinding,
    pub required_schema_version: u64,
    pub filename: String,
    pub cdc_ctx: Option<SyncContext>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeSinkPayload {
    pub request_id: u64,
    pub arrow_stream_bytes: Vec<u8>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SchemaRunRequest {
    pub request_id: u64,
    pub compaction_id: String,
    pub binding: RuntimeBinding,
    pub required_schema_version: u64,
    pub namespace: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum HostFrame {
    Handshake(HandshakeRequest),
    InstallSink(RuntimeSinkInstallRequest),
    InstallSchema(RuntimeSchemaInstallRequest),
    InstallSchemaState(RuntimeSchemaStateInstallRequest),
    RunSource(SourceStartRequest),
    RunSink(SinkRunRequest),
    RunSchema(SchemaRunRequest),
    OffsetResponse(RuntimeOffsetRpcResponse),
    Shutdown,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum PluginFrame {
    HandshakeAck(HandshakeResponse),
    Installed,
    SourceEvent(SourceEvent),
    OffsetRequest(RuntimeOffsetRpcRequest),
    SinkAck(RuntimeRequestAck),
    SchemaAck(RuntimeRequestAck),
    SchemaStateRefreshRequired(RuntimeSchemaRefreshRequest),
    Error(String),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum HostDataFrame {
    SinkPayload(RuntimeSinkPayload),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum PluginDataFrame {
    IngestBatches {
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
