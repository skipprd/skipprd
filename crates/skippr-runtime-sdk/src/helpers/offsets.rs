use serde_derive::{Deserialize, Serialize};

use crate::plugins::cdc::CheckpointEnvelope;

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub struct OffsetKey {
    pub namespace: String,
    pub partition: String,
}

impl OffsetKey {
    pub fn new(namespace: impl Into<String>, partition: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            partition: partition.into(),
        }
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub fn partition(&self) -> &str {
        &self.partition
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum OffsetTypes {
    Filesize,
    Position,
    Closed,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub enum RuntimeOffsetOperation {
    Validate {
        key: OffsetKey,
        offset_type: OffsetTypes,
        offset_value: u64,
    },
    LoadCheckpointEnvelope {
        key: String,
    },
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct RuntimeOffsetRpcRequest {
    pub request_id: u64,
    pub operation: RuntimeOffsetOperation,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub enum RuntimeOffsetValue {
    Validate(Option<bool>),
    LoadCheckpointEnvelope(Option<CheckpointEnvelope>),
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct RuntimeOffsetRpcResponse {
    pub request_id: u64,
    pub result: Result<RuntimeOffsetValue, String>,
}

pub trait OffsetTransport: Send + Sync {
    fn call(&self, operation: RuntimeOffsetOperation) -> Result<RuntimeOffsetValue, String>;
}

pub trait CheckpointTransport: Send + Sync {
    fn store_checkpoint(&self, key: &str, envelope: &CheckpointEnvelope) -> Result<(), String>;
    fn load_checkpoint_envelope(&self, key: &str) -> Result<Option<CheckpointEnvelope>, String>;
}
