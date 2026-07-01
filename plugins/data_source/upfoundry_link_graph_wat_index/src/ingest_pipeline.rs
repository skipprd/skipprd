use std::sync::Arc;

use skippr_runtime_sdk::helpers::offsets::OffsetKey;
use skippr_runtime_sdk::plugins::SourceSyncContext;
use skippr_runtime_sdk::protocol::{RuntimeIngestPartitionBatch, RuntimeOffsetPosition};
use skippr_runtime_sdk::source_compat::PayloadSubmissionBatch;

use crate::arrow_batch::{encode_record_batch_ipc, namespace_label, TargetIndexBatchBuilder};
use crate::job::path_offset_partition;
use crate::streams::NAMESPACE_TARGET_INDEX;

fn env_string(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn env_truthy(name: &str) -> bool {
    env_string(name).is_some_and(|value| {
        value == "1"
            || value.eq_ignore_ascii_case("true")
            || value.eq_ignore_ascii_case("yes")
            || value.eq_ignore_ascii_case("on")
    })
}

fn runtime_is_ci() -> bool {
    env_truthy("GITHUB_ACTIONS") || env_truthy("CI")
}

fn ingest_threads() -> usize {
    env_string("INGEST_THREADS")
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or_else(|| num_cpus::get().max(1))
}

pub fn ingest_pending_max_bytes() -> usize {
    env_string("INGEST_PENDING_MAX_BYTES")
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or_else(|| {
            if runtime_is_ci() {
                512 * 1024 * 1024
            } else {
                2 * 1024 * 1024 * 1024
            }
        })
}

pub fn source_pending_max_bytes() -> usize {
    env_string("WAT_INDEX_SOURCE_PENDING_MAX_BYTES")
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or_else(|| {
            if runtime_is_ci() {
                64 * 1024 * 1024
            } else {
                512 * 1024 * 1024
            }
        })
}

fn accepted_submission_window() -> usize {
    env_string("WAT_INDEX_ACCEPTED_SUBMISSION_WINDOW")
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(128)
}

pub struct ArrowIngestPipeline {
    ctx: Arc<dyn SourceSyncContext>,
    dispatch_cpus: usize,
    pending_cap: usize,
    accepted_window: usize,
    pending_batches: Vec<RuntimeIngestPartitionBatch>,
    accepted_submissions: Vec<PayloadSubmissionBatch>,
    pending_bytes_sum: usize,
}

impl ArrowIngestPipeline {
    pub fn new(ctx: Arc<dyn SourceSyncContext>) -> Self {
        Self {
            ctx,
            dispatch_cpus: ingest_threads(),
            pending_cap: ingest_pending_max_bytes(),
            accepted_window: accepted_submission_window(),
            pending_batches: Vec::with_capacity(ingest_threads()),
            accepted_submissions: Vec::new(),
            pending_bytes_sum: 0,
        }
    }

    pub fn queue_arrow_batch(
        &mut self,
        crawl_id: &str,
        bucket: u32,
        path_index: usize,
        member_index: u64,
        builder: &mut TargetIndexBatchBuilder,
    ) -> Result<(), std::io::Error> {
        let Some(batch) = builder
            .finish_record_batch()
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err.to_string()))?
        else {
            return Ok(());
        };
        let arrow_stream_bytes = encode_record_batch_ipc(&batch)?;
        let bytes = arrow_stream_bytes.len();
        let partition = format!("{crawl_id}#{bucket:05}");
        let path_partition = path_offset_partition(crawl_id, path_index);
        self.pending_batches.push(RuntimeIngestPartitionBatch {
            sink_ref: partition.clone(),
            namespace: NAMESPACE_TARGET_INDEX.to_string(),
            partition,
            time: None,
            schema_fingerprint: String::new(),
            offsets: vec![
                RuntimeOffsetPosition {
                    key: OffsetKey::new(namespace_label(), path_partition),
                    position: member_index,
                },
                RuntimeOffsetPosition {
                    key: OffsetKey::new(namespace_label(), format!("{crawl_id}#{bucket:05}")),
                    position: member_index,
                },
            ],
            arrow_stream_bytes,
            cdc_rows: None,
            checkpoint_update: None,
        });
        self.pending_bytes_sum = self.pending_bytes_sum.saturating_add(bytes);
        self.dispatch_if_ready()
    }

    fn dispatch_if_ready(&mut self) -> Result<(), std::io::Error> {
        if self.pending_batches.len() < self.dispatch_cpus
            && self.pending_bytes_sum < self.pending_cap
        {
            return Ok(());
        }
        self.dispatch_pending()
    }

    fn dispatch_pending(&mut self) -> Result<(), std::io::Error> {
        if self.pending_batches.is_empty() {
            return Ok(());
        }
        let submission = self
            .ctx
            .submit_arrow_ipc_batches_accepted(std::mem::take(&mut self.pending_batches))?;
        self.accepted_submissions.push(submission);
        self.pending_bytes_sum = 0;
        if self.accepted_submissions.len() >= self.accepted_window {
            self.ctx.wait_payload_acks(&self.accepted_submissions)?;
            self.accepted_submissions.clear();
        }
        Ok(())
    }

    pub fn await_durable_submissions(&mut self) -> Result<(), std::io::Error> {
        self.dispatch_pending()?;
        if !self.accepted_submissions.is_empty() {
            self.ctx.wait_payload_acks(&self.accepted_submissions)?;
            self.accepted_submissions.clear();
        }
        Ok(())
    }

    pub fn flush_pending(&mut self) -> Result<(), std::io::Error> {
        self.dispatch_pending()
    }

    pub fn finish(mut self) -> Result<(), std::io::Error> {
        self.dispatch_pending()?;
        if !self.accepted_submissions.is_empty() {
            self.ctx.wait_payload_acks(&self.accepted_submissions)?;
        }
        self.ctx.drain_payload_acks()
    }
}

pub fn ingest_backpressure_pressure(ctx: &dyn SourceSyncContext) -> f64 {
    let (requests, bytes) = ctx.payload_in_flight();
    let request_pressure = requests as f64 / 64.0;
    let byte_pressure = bytes as f64 / (512.0 * 1024.0 * 1024.0);
    request_pressure.max(byte_pressure)
}
