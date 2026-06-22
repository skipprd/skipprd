use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Condvar, Mutex as StdMutex};

use skippr_core::helpers::offsets::{OffsetKey, OffsetTypes};
use skippr_core::ingest_work::{IngestBatch, ThroughputMetrics};
use skippr_core::plugins::cdc::{CheckpointAuthority, CheckpointEnvelope, CheckpointKind};
use skippr_core::plugins::source_sync::{
    OffsetValidationEntry, PayloadSubmissionBatch, SourcePayloadTask, SourceSyncContext,
};
use skippr_core::runtime_plugins::protocol::{
    HostOffsetFrame, PluginDataFrame, PluginOffsetFrame, RuntimeCheckpointUpdate, RuntimeIngestAck,
    RuntimeOffsetMaterializationHint, RuntimeOffsetValidationEntry, RuntimeRawIngestBatch,
    RuntimeSessionHello, RuntimeSourceIngestWindow, RUNTIME_PROTOCOL_VERSION,
    SKIPPR_RUNTIME_OFFSET_ADDR_ENV, SKIPPR_RUNTIME_SESSION_TOKEN_ENV,
};
use skippr_core::runtime_plugins::wire::{read_frame, write_frame};
use tokio::net::TcpStream;
use tokio::runtime::Handle;
use tokio::sync::Mutex;

use crate::append_source_runtime::{block_on_handle, ControlWriter, DataWriter};

/// Keep host-ingest frames under the runtime wire limit (see `wire::MAX_RUNTIME_FRAME_BYTES`).
const MAX_SOURCE_PAYLOAD_FRAME_BYTES: usize = 256 * 1024 * 1024;
const SOURCE_PAYLOAD_FRAME_HEADROOM: usize = 64 * 1024;

fn chunk_source_payload_tasks(
    runtime_tasks: Vec<Vec<RuntimeRawIngestBatch>>,
) -> Vec<Vec<Vec<RuntimeRawIngestBatch>>> {
    let limit = MAX_SOURCE_PAYLOAD_FRAME_BYTES.saturating_sub(SOURCE_PAYLOAD_FRAME_HEADROOM);
    let mut chunks: Vec<Vec<Vec<RuntimeRawIngestBatch>>> = Vec::new();
    let mut current: Vec<Vec<RuntimeRawIngestBatch>> = Vec::new();

    for task in runtime_tasks {
        current.push(task);
        let encoded_len = bincode::serialize(&PluginDataFrame::SourcePayloadBatches {
            request_id: 0,
            tasks: current.clone(),
        })
        .map(|bytes| bytes.len())
        .unwrap_or(limit.saturating_add(1));
        if encoded_len > limit {
            let overflow = current
                .pop()
                .expect("chunk probe always has at least one task");
            if !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
            }
            current.push(overflow);
        }
    }

    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

struct IngestAckState {
    pending: HashMap<u64, usize>,
    completed: HashMap<u64, Result<(), String>>,
    in_flight_requests: usize,
    in_flight_bytes: usize,
}

pub(crate) struct RuntimeIngestAckClient {
    state: Arc<(StdMutex<IngestAckState>, Condvar)>,
    next_request_id: AtomicU64,
    max_in_flight_requests: usize,
    max_in_flight_bytes: usize,
}

impl RuntimeIngestAckClient {
    pub(crate) fn new(window: RuntimeSourceIngestWindow) -> Self {
        let max_in_flight_requests = window.max_in_flight_requests.clamp(1, 64);
        let max_in_flight_bytes = window.max_in_flight_bytes.max(1024 * 1024);
        Self {
            state: Arc::new((
                StdMutex::new(IngestAckState {
                    pending: HashMap::new(),
                    completed: HashMap::new(),
                    in_flight_requests: 0,
                    in_flight_bytes: 0,
                }),
                Condvar::new(),
            )),
            next_request_id: AtomicU64::new(1),
            max_in_flight_requests,
            max_in_flight_bytes,
        }
    }

    fn next_request_id(&self) -> u64 {
        self.next_request_id.fetch_add(1, Ordering::Relaxed)
    }

    fn wait_for_capacity(&self, request_bytes: usize) {
        let (lock, cv) = &*self.state;
        let mut guard = lock.lock().unwrap();
        while guard.in_flight_requests >= self.max_in_flight_requests
            || (guard.in_flight_bytes > 0
                && guard.in_flight_bytes.saturating_add(request_bytes) > self.max_in_flight_bytes)
        {
            guard = cv.wait(guard).unwrap();
        }
    }

    fn register_request(&self, request_id: u64, request_bytes: usize) {
        let (lock, cv) = &*self.state;
        let mut guard = lock.lock().unwrap();
        guard.pending.insert(request_id, request_bytes);
        guard.in_flight_requests = guard.in_flight_requests.saturating_add(1);
        guard.in_flight_bytes = guard.in_flight_bytes.saturating_add(request_bytes);
        cv.notify_all();
    }

    pub(crate) fn resolve_ack(&self, ack: RuntimeIngestAck) {
        let (lock, cv) = &*self.state;
        let mut guard = lock.lock().unwrap();
        if let Some(bytes) = guard.pending.remove(&ack.request_id) {
            guard.in_flight_requests = guard.in_flight_requests.saturating_sub(1);
            guard.in_flight_bytes = guard.in_flight_bytes.saturating_sub(bytes);
        }
        let result = ack.error.map_or(Ok(()), Err);
        guard.completed.insert(ack.request_id, result);
        cv.notify_all();
    }

    pub(crate) fn fail_all(&self, message: String) {
        let (lock, cv) = &*self.state;
        let mut guard = lock.lock().unwrap();
        let pending = std::mem::take(&mut guard.pending);
        for (request_id, _) in pending {
            guard.completed.insert(request_id, Err(message.clone()));
        }
        guard.in_flight_requests = 0;
        guard.in_flight_bytes = 0;
        cv.notify_all();
    }

    fn wait_for_request_ids(&self, request_ids: &[u64]) -> Result<(), io::Error> {
        let mut remaining: HashSet<u64> = request_ids.iter().copied().collect();
        let (lock, cv) = &*self.state;
        let mut guard = lock.lock().unwrap();
        while !remaining.is_empty() {
            let completed_ids = remaining
                .iter()
                .copied()
                .filter(|request_id| guard.completed.contains_key(request_id))
                .collect::<Vec<_>>();
            for request_id in completed_ids {
                remaining.remove(&request_id);
                match guard.completed.remove(&request_id).unwrap() {
                    Ok(()) => {}
                    Err(err) => return Err(io::Error::other(err)),
                }
            }
            if !remaining.is_empty() {
                guard = cv.wait(guard).unwrap();
            }
        }
        Ok(())
    }

    fn drain(&self) -> Result<(), io::Error> {
        let (lock, cv) = &*self.state;
        let mut guard = lock.lock().unwrap();
        while !guard.pending.is_empty() {
            guard = cv.wait(guard).unwrap();
        }
        let completed = std::mem::take(&mut guard.completed);
        for (_, result) in completed {
            result.map_err(io::Error::other)?;
        }
        Ok(())
    }

    fn in_flight(&self) -> (usize, usize) {
        let guard = self.state.0.lock().unwrap();
        (guard.in_flight_requests, guard.in_flight_bytes)
    }
}

enum OffsetServiceResponse {
    Validate(Vec<bool>),
    LoadCheckpoint(Option<CheckpointEnvelope>),
}

pub(crate) struct RuntimeOffsetClient {
    handle: Handle,
    writer: Arc<Mutex<tokio::net::tcp::OwnedWriteHalf>>,
    pending: Arc<StdMutex<HashMap<u64, SyncSender<Result<OffsetServiceResponse, String>>>>>,
    next_request_id: AtomicU64,
}

impl RuntimeOffsetClient {
    async fn connect() -> io::Result<(Self, tokio::net::tcp::OwnedReadHalf)> {
        let addr = std::env::var(SKIPPR_RUNTIME_OFFSET_ADDR_ENV).map_err(|_| {
            io::Error::other(format!(
                "missing {} for runtime source",
                SKIPPR_RUNTIME_OFFSET_ADDR_ENV
            ))
        })?;
        let token = std::env::var(SKIPPR_RUNTIME_SESSION_TOKEN_ENV).map_err(|_| {
            io::Error::other(format!(
                "missing {} for runtime source",
                SKIPPR_RUNTIME_SESSION_TOKEN_ENV
            ))
        })?;
        let mut stream = TcpStream::connect(addr).await?;
        let hello = RuntimeSessionHello {
            protocol_version: RUNTIME_PROTOCOL_VERSION,
            token,
        };
        write_frame(&mut stream, &hello).await?;
        let (reader, writer) = stream.into_split();
        let client = Self {
            handle: Handle::current(),
            writer: Arc::new(Mutex::new(writer)),
            pending: Arc::new(StdMutex::new(HashMap::new())),
            next_request_id: AtomicU64::new(1),
        };
        Ok((client, reader))
    }

    fn register_request(
        &self,
        request_id: u64,
        tx: SyncSender<Result<OffsetServiceResponse, String>>,
    ) {
        self.pending.lock().unwrap().insert(request_id, tx);
    }

    fn resolve_response(&self, response: HostOffsetFrame) {
        let (request_id, payload) = match response {
            HostOffsetFrame::ValidateOffsetBatchResponse {
                request_id,
                should_process,
            } => (request_id, OffsetServiceResponse::Validate(should_process)),
            HostOffsetFrame::LoadCheckpointResponse {
                request_id,
                envelope,
            } => (request_id, OffsetServiceResponse::LoadCheckpoint(envelope)),
        };
        if let Some(tx) = self.pending.lock().unwrap().remove(&request_id) {
            let _ = tx.send(Ok(payload));
        }
    }

    async fn send_validate_batch(
        &self,
        entries: Vec<RuntimeOffsetValidationEntry>,
    ) -> io::Result<Vec<bool>> {
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.register_request(request_id, tx);
        let frame = PluginOffsetFrame::ValidateOffsetBatch {
            request_id,
            entries,
        };
        {
            let mut guard = self.writer.lock().await;
            write_frame(&mut *guard, &frame).await?;
        }
        let response = rx
            .recv()
            .map_err(|err| io::Error::other(format!("offset service response dropped: {err}")))?
            .map_err(io::Error::other)?;
        match response {
            OffsetServiceResponse::Validate(should_process) => Ok(should_process),
            OffsetServiceResponse::LoadCheckpoint(_) => Err(io::Error::other(
                "offset service returned checkpoint response for validate request",
            )),
        }
    }

    async fn send_load_checkpoint(&self, key: String) -> io::Result<Option<CheckpointEnvelope>> {
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.register_request(request_id, tx);
        let frame = PluginOffsetFrame::LoadCheckpoint { request_id, key };
        {
            let mut guard = self.writer.lock().await;
            write_frame(&mut *guard, &frame).await?;
        }
        let response = rx
            .recv()
            .map_err(|err| io::Error::other(format!("offset service response dropped: {err}")))?
            .map_err(io::Error::other)?;
        match response {
            OffsetServiceResponse::LoadCheckpoint(envelope) => Ok(envelope),
            OffsetServiceResponse::Validate(_) => Err(io::Error::other(
                "offset service returned validate response for checkpoint request",
            )),
        }
    }
}

pub(crate) async fn run_offset_service_reader_loop(
    mut reader: tokio::net::tcp::OwnedReadHalf,
    client: Arc<RuntimeOffsetClient>,
) -> io::Result<()> {
    loop {
        let frame: HostOffsetFrame = match read_frame(&mut reader).await {
            Ok(frame) => frame,
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(err) => return Err(err),
        };
        client.resolve_response(frame);
    }
}

pub struct RuntimeSourceSyncContext {
    control_writer: ControlWriter,
    data_writer: DataWriter,
    offset_client: Arc<RuntimeOffsetClient>,
    ingest_ack_client: Arc<RuntimeIngestAckClient>,
    suppress_payloads: bool,
    metrics: Arc<StdMutex<ThroughputMetrics>>,
}

impl RuntimeSourceSyncContext {
    pub(crate) async fn new(
        control_writer: ControlWriter,
        data_writer: DataWriter,
        ingest_ack_client: Arc<RuntimeIngestAckClient>,
        suppress_payloads: bool,
    ) -> io::Result<(Self, tokio::net::tcp::OwnedReadHalf)> {
        let (offset_client, offset_reader) = RuntimeOffsetClient::connect().await?;
        let offset_client = Arc::new(offset_client);
        Ok((
            Self {
                control_writer,
                data_writer,
                offset_client,
                ingest_ack_client,
                suppress_payloads,
                metrics: Arc::new(StdMutex::new(ThroughputMetrics {
                    bytes_per_second: 0,
                    active_cores: 0,
                    queue_length: 0,
                    optimal_chunk_size: 0,
                })),
            },
            offset_reader,
        ))
    }

    pub(crate) fn offset_client(&self) -> Arc<RuntimeOffsetClient> {
        self.offset_client.clone()
    }

    pub fn last_metrics(&self) -> ThroughputMetrics {
        self.metrics.lock().unwrap().clone()
    }
}

impl SourceSyncContext for RuntimeSourceSyncContext {
    fn submit_payload_tasks(
        &self,
        tasks: Vec<SourcePayloadTask>,
    ) -> Result<ThroughputMetrics, io::Error> {
        self.submit_payload_tasks_accepted(tasks)
            .map(|submission| submission.metrics)
    }

    fn submit_payload_tasks_accepted(
        &self,
        tasks: Vec<SourcePayloadTask>,
    ) -> Result<PayloadSubmissionBatch, io::Error> {
        if self.suppress_payloads || tasks.is_empty() {
            return Ok(PayloadSubmissionBatch::already_durable(self.last_metrics()));
        }

        let runtime_tasks = tasks
            .into_iter()
            .map(|task| {
                task.batches
                    .into_iter()
                    .map(|batch| RuntimeRawIngestBatch {
                        offset_key: batch.offset_key,
                        data: batch.data,
                        bytes: batch.bytes,
                        offset_pos: batch.offset_pos,
                        source_uri: batch.source_uri,
                        namespace: batch.namespace,
                        cdc_rows: batch.cdc_rows,
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

        let mut request_ids = Vec::new();
        let total_bytes = runtime_tasks
            .iter()
            .flat_map(|task| task.iter())
            .map(|batch| batch.bytes)
            .sum::<usize>();

        for chunk in chunk_source_payload_tasks(runtime_tasks) {
            let request_id = self.ingest_ack_client.next_request_id();
            let request_bytes = chunk
                .iter()
                .flat_map(|task| task.iter())
                .map(|batch| batch.bytes)
                .sum::<usize>();
            self.ingest_ack_client.wait_for_capacity(request_bytes);
            self.ingest_ack_client
                .register_request(request_id, request_bytes);
            if let Err(err) = block_on_handle(&self.control_writer.handle, async {
                self.data_writer
                    .write(&PluginDataFrame::SourcePayloadBatches {
                        request_id,
                        tasks: chunk,
                    })
                    .await
            }) {
                self.ingest_ack_client.resolve_ack(RuntimeIngestAck {
                    request_id,
                    error: Some(format!("failed to submit runtime ingest payload: {err}")),
                });
                return Err(err);
            }
            request_ids.push(request_id);
        }

        Ok(PayloadSubmissionBatch {
            request_ids,
            bytes: total_bytes,
            metrics: self.last_metrics(),
        })
    }

    fn wait_payload_acks(&self, submissions: &[PayloadSubmissionBatch]) -> Result<(), io::Error> {
        let request_ids = submissions
            .iter()
            .flat_map(|submission| submission.request_ids.iter().copied())
            .collect::<Vec<_>>();
        self.ingest_ack_client.wait_for_request_ids(&request_ids)
    }

    fn drain_payload_acks(&self) -> Result<(), io::Error> {
        self.ingest_ack_client.drain()
    }

    fn payload_in_flight(&self) -> (usize, usize) {
        self.ingest_ack_client.in_flight()
    }

    fn validate_offset_batch(
        &self,
        entries: &[OffsetValidationEntry],
    ) -> Result<Vec<bool>, io::Error> {
        if entries.is_empty() {
            return Ok(Vec::new());
        }
        let payload = entries
            .iter()
            .map(|entry| RuntimeOffsetValidationEntry {
                key: entry.key.clone(),
                offset_type: entry.offset_type,
                offset_value: entry.offset_value,
            })
            .collect();
        block_on_handle(&self.offset_client.handle, async {
            self.offset_client.send_validate_batch(payload).await
        })
    }

    fn relay_offset_hints(
        &self,
        hints: Vec<RuntimeOffsetMaterializationHint>,
    ) -> Result<(), io::Error> {
        if self.suppress_payloads || hints.is_empty() {
            return Ok(());
        }
        block_on_handle(&self.control_writer.handle, async {
            self.data_writer
                .write(&PluginDataFrame::OffsetMaterializationHints { hints })
                .await
        })
    }

    fn store_checkpoint(&self, key: &str, envelope: &CheckpointEnvelope) -> Result<(), String> {
        if self.suppress_payloads {
            return Ok(());
        }
        block_on_handle(&self.control_writer.handle, async {
            self.data_writer
                .write(&PluginDataFrame::CheckpointUpdate {
                    update: RuntimeCheckpointUpdate {
                        key: key.to_string(),
                        envelope: envelope.clone(),
                    },
                })
                .await
                .map_err(|err| err.to_string())
        })
    }

    fn load_checkpoint_envelope(&self, key: &str) -> Option<CheckpointEnvelope> {
        block_on_handle(&self.offset_client.handle, async {
            self.offset_client
                .send_load_checkpoint(key.to_string())
                .await
        })
        .ok()
        .flatten()
    }
}

/// Submit one schedulable batch group to host-owned ingest.
pub fn submit_payload_batches(
    ctx: &dyn SourceSyncContext,
    batches: Vec<IngestBatch>,
) -> Result<ThroughputMetrics, io::Error> {
    if batches.is_empty() {
        return Ok(ThroughputMetrics {
            bytes_per_second: 0,
            active_cores: 0,
            queue_length: 0,
            optimal_chunk_size: 0,
        });
    }
    ctx.submit_payload_tasks_and_wait(vec![source_payload_task(batches)])
}

/// Submit multiple schedulable batch groups to host-owned ingest.
pub fn submit_payload_batch_groups(
    ctx: &dyn SourceSyncContext,
    groups: Vec<Vec<IngestBatch>>,
) -> Result<ThroughputMetrics, io::Error> {
    if groups.is_empty() {
        return Ok(ThroughputMetrics {
            bytes_per_second: 0,
            active_cores: 0,
            queue_length: 0,
            optimal_chunk_size: 0,
        });
    }
    let tasks = groups
        .into_iter()
        .map(source_payload_task)
        .collect::<Vec<_>>();
    ctx.submit_payload_tasks_and_wait(tasks)
}

/// Returns true when a Closed partition was already ingested and should be skipped.
pub fn partition_already_closed(ctx: &dyn SourceSyncContext, key: &OffsetKey) -> bool {
    !validate_offset_key(ctx, key, OffsetTypes::Closed, 1).unwrap_or(true)
}

/// Validate a single offset key through the batched offset service.
///
/// For Closed offsets this returns whether the partition should still be processed.
pub fn validate_offset_key(
    ctx: &dyn SourceSyncContext,
    key: &OffsetKey,
    offset_type: OffsetTypes,
    offset_value: u64,
) -> Option<bool> {
    let entry = offset_validation_entry(
        key.namespace.clone(),
        key.partition.clone(),
        offset_type,
        offset_value,
    );
    ctx.validate_offset_batch(&[entry])
        .ok()
        .and_then(|mut results| results.pop())
}

pub const CHECKPOINT_PAYLOAD_VERSION: u32 = 1;

/// Load a typed checkpoint payload from the host offset store.
pub fn load_checkpoint_payload<T: serde::de::DeserializeOwned>(
    ctx: &dyn SourceSyncContext,
    key: &str,
) -> Option<T> {
    ctx.load_checkpoint_envelope(key)
        .and_then(|envelope| envelope.into_payload().ok())
}

/// Persist a typed checkpoint payload to the host offset store.
pub fn store_checkpoint_payload<T: serde::Serialize>(
    ctx: &dyn SourceSyncContext,
    key: &str,
    payload: &T,
) -> io::Result<()> {
    let envelope = CheckpointEnvelope::from_payload(
        CheckpointAuthority::AdvisoryHint,
        CheckpointKind::SourceResume,
        CHECKPOINT_PAYLOAD_VERSION,
        payload,
    )
    .map_err(|e| io::Error::other(e.to_string()))?;
    ctx.store_checkpoint(key, &envelope)
        .map_err(io::Error::other)
}

pub fn offset_validation_entry(
    namespace: impl Into<String>,
    partition: impl Into<String>,
    offset_type: OffsetTypes,
    offset_value: u64,
) -> OffsetValidationEntry {
    OffsetValidationEntry {
        key: skippr_core::helpers::offsets::OffsetKey::new(namespace, partition),
        offset_type,
        offset_value,
    }
}

pub fn source_payload_task(batches: Vec<IngestBatch>) -> SourcePayloadTask {
    SourcePayloadTask { batches }
}

#[cfg(test)]
mod tests {
    use super::*;
    use skippr_core::runtime_plugins::protocol::RuntimeSourceIngestWindow;

    fn test_ack_client() -> RuntimeIngestAckClient {
        RuntimeIngestAckClient::new(RuntimeSourceIngestWindow {
            max_in_flight_requests: 2,
            max_in_flight_bytes: 1024,
        })
    }

    #[test]
    fn ingest_ack_client_propagates_nack() {
        let client = test_ack_client();
        client.register_request(7, 128);
        assert_eq!(client.in_flight(), (1, 128));

        client.resolve_ack(RuntimeIngestAck {
            request_id: 7,
            error: Some("wal failed".to_string()),
        });

        let err = client.wait_for_request_ids(&[7]).unwrap_err();
        assert!(err.to_string().contains("wal failed"));
        assert_eq!(client.in_flight(), (0, 0));
    }

    #[test]
    fn ingest_ack_client_drains_successful_completions() {
        let client = test_ack_client();
        client.register_request(8, 64);
        client.resolve_ack(RuntimeIngestAck {
            request_id: 8,
            error: None,
        });

        client.drain().unwrap();
        assert_eq!(client.in_flight(), (0, 0));
    }

    #[test]
    fn ingest_ack_client_fail_all_completes_pending_requests() {
        let client = test_ack_client();
        client.register_request(9, 64);
        client.register_request(10, 64);

        client.fail_all("host stopped".to_string());

        let err = client.wait_for_request_ids(&[9, 10]).unwrap_err();
        assert!(err.to_string().contains("host stopped"));
        assert_eq!(client.in_flight(), (0, 0));
    }

    #[test]
    fn ingest_ack_client_allows_oversized_request_when_window_empty() {
        let client = test_ack_client();
        client.wait_for_capacity(2048);
        client.register_request(11, 2048);
        assert_eq!(client.in_flight(), (1, 2048));

        client.resolve_ack(RuntimeIngestAck {
            request_id: 11,
            error: None,
        });
        client.drain().unwrap();
    }
}
