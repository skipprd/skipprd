use serde::{Deserialize, Serialize};

use crate::discover::OutputMetadata;
use crate::helpers::configuration::{DataSinkPluginConfig, DataSourcePluginConfig};
use crate::ingest_work::IngestBatch;
use crate::plugins::cdc::{
    CheckpointEnvelope, EventIdSemantics, SinkCapability, SinkGuaranteeTier, SourceBootstrapStyle,
    SourceCapability, SourceCheckpointStyle, SourceGuaranteeTier, SourceOrderModel, SyncContext,
};

pub const RUNTIME_PROTOCOL_VERSION: u32 = 1;

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
    pub config: RuntimeSourceConfig,
    pub resume_checkpoint: Option<CheckpointEnvelope>,
    pub legacy_resume_bytes: Option<Vec<u8>>,
    pub bootstrap_anchor: Option<CheckpointEnvelope>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeCheckpointUpdate {
    pub key: String,
    pub envelope: CheckpointEnvelope,
    pub legacy_payload_bytes: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum SourceEvent {
    IngestBatches {
        batches: Vec<IngestBatch>,
    },
    SinkWrite {
        filename: String,
        arrow_stream_bytes: Vec<u8>,
        cdc_ctx: Option<SyncContext>,
    },
    CheckpointUpdate(RuntimeCheckpointUpdate),
    Completed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SinkRunRequest {
    pub config: RuntimeSinkConfig,
    pub binding: RuntimeBinding,
    pub filename: String,
    pub arrow_stream_bytes: Vec<u8>,
    pub cdc_ctx: Option<SyncContext>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SchemaRunRequest {
    pub config: RuntimeSchemaConfig,
    pub binding: RuntimeBinding,
    pub namespace: String,
    pub metadata: OutputMetadata,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum HostFrame {
    Handshake(HandshakeRequest),
    RunSource(SourceStartRequest),
    RunSink(SinkRunRequest),
    RunSchema(SchemaRunRequest),
    Shutdown,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum PluginFrame {
    HandshakeAck(HandshakeResponse),
    SourceEvent(SourceEvent),
    SinkAck,
    SchemaAck,
    Error(String),
}
