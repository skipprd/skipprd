use std::io;

use crate::helpers::offsets::{OffsetKey, OffsetTypes};
use crate::ingest_work::{IngestBatch, ThroughputMetrics};
use crate::plugins::cdc::CheckpointEnvelope;
use crate::runtime_plugins::protocol::{
    RuntimeIngestPartitionBatch, RuntimeOffsetMaterializationHint,
};

/// One schedulable unit of source payload for host-owned ingest.
#[derive(Clone, Debug)]
pub struct SourcePayloadTask {
    pub batches: Vec<IngestBatch>,
}

#[derive(Clone, Debug)]
pub struct PayloadSubmissionBatch {
    pub request_ids: Vec<u64>,
    pub bytes: usize,
    pub metrics: ThroughputMetrics,
}

impl PayloadSubmissionBatch {
    pub fn already_durable(metrics: ThroughputMetrics) -> Self {
        Self {
            request_ids: Vec::new(),
            bytes: 0,
            metrics,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PayloadAck {
    pub request_id: u64,
    pub result: Result<(), String>,
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

    /// Submit payload tasks and return once the host has accepted them.
    ///
    /// Implementations that do not support asynchronous ACKs can keep the old
    /// behavior by using the default implementation, which waits for durability
    /// via `submit_payload_tasks`.
    fn submit_payload_tasks_accepted(
        &self,
        tasks: Vec<SourcePayloadTask>,
    ) -> Result<PayloadSubmissionBatch, io::Error> {
        self.submit_payload_tasks(tasks)
            .map(PayloadSubmissionBatch::already_durable)
    }

    /// Submit already-normalized Arrow IPC partition batches.
    ///
    /// Implementations that support runtime async ACKs should return once the
    /// host accepts the request and complete the ACK only after durable WAL
    /// persistence. Implementations that do not support Arrow IPC return an
    /// error rather than silently falling back to a different semantic path.
    fn submit_arrow_ipc_batches_accepted(
        &self,
        batches: Vec<RuntimeIngestPartitionBatch>,
    ) -> Result<PayloadSubmissionBatch, io::Error> {
        let _ = batches;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Arrow IPC source payloads require runtime source context support",
        ))
    }

    /// Compatibility helper for callers that still need submit to be a durability barrier.
    fn submit_payload_tasks_and_wait(
        &self,
        tasks: Vec<SourcePayloadTask>,
    ) -> Result<ThroughputMetrics, io::Error> {
        let submission = self.submit_payload_tasks_accepted(tasks)?;
        self.wait_payload_acks(std::slice::from_ref(&submission))?;
        Ok(submission.metrics)
    }

    /// Wait until the listed accepted payload submissions are WAL-durable.
    fn wait_payload_acks(&self, submissions: &[PayloadSubmissionBatch]) -> Result<(), io::Error> {
        let _ = submissions;
        Ok(())
    }

    /// Drain any accepted payload submissions that are still waiting for durability.
    fn drain_payload_acks(&self) -> Result<(), io::Error> {
        Ok(())
    }

    /// Return current source-side in-flight payload request count and byte budget.
    fn payload_in_flight(&self) -> (usize, usize) {
        (0, 0)
    }

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
