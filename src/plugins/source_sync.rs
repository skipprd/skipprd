use std::io;

use crate::helpers::offsets::{OffsetKey, OffsetTypes};
use crate::ingest_work::{IngestBatch, ThroughputMetrics};
use crate::plugins::cdc::CheckpointEnvelope;
use crate::runtime_plugins::protocol::RuntimeOffsetMaterializationHint;

/// One schedulable unit of source payload for host-owned ingest.
#[derive(Clone, Debug)]
pub struct SourcePayloadTask {
    pub batches: Vec<IngestBatch>,
}

/// Generic offset validation request entry. Source plugins map domain keys into this shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OffsetValidationEntry {
    pub key: OffsetKey,
    pub offset_type: OffsetTypes,
    pub offset_value: u64,
}

/// Runtime and in-process sources submit payloads and offset checks through this context.
pub trait SourceSyncContext: Send + Sync {
    /// Submit one or more payload tasks to the host ingest scheduler.
    fn submit_payload_tasks(
        &self,
        tasks: Vec<SourcePayloadTask>,
    ) -> Result<ThroughputMetrics, io::Error>;

    /// Batch offset validation for list-time filtering and similar hot paths.
    fn validate_offset_batch(
        &self,
        entries: &[OffsetValidationEntry],
    ) -> Result<Vec<bool>, io::Error>;

    /// Emit coarse offset materialization hints after source-side progress.
    fn relay_offset_hints(
        &self,
        hints: Vec<RuntimeOffsetMaterializationHint>,
    ) -> Result<(), io::Error>;

    /// Store a source checkpoint envelope through the host-owned durability path.
    fn store_checkpoint(&self, key: &str, envelope: &CheckpointEnvelope) -> Result<(), String>;

    /// Load a durable checkpoint envelope from the host offset store.
    fn load_checkpoint_envelope(&self, key: &str) -> Option<CheckpointEnvelope>;
}
