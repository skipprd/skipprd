use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex as StdMutex};

use skippr_core::helpers::offsets::{OffsetKey, OffsetTypes};
use skippr_core::ingest_work::{IngestBatch, ThroughputMetrics};
use skippr_core::plugins::cdc::{CheckpointAuthority, CheckpointEnvelope, CheckpointKind};
use skippr_core::plugins::source_sync::{
    OffsetValidationEntry, SourcePayloadTask, SourceSyncContext,
};
use skippr_core::runtime_plugins::protocol::{
    HostOffsetFrame, PluginDataFrame, PluginOffsetFrame, RuntimeCheckpointUpdate,
    RuntimeOffsetMaterializationHint, RuntimeOffsetValidationEntry, RuntimeRawIngestBatch,
    RuntimeSessionHello, RUNTIME_PROTOCOL_VERSION, SKIPPR_RUNTIME_OFFSET_ADDR_ENV,
    SKIPPR_RUNTIME_SESSION_TOKEN_ENV,
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
    suppress_payloads: bool,
    metrics: Arc<StdMutex<ThroughputMetrics>>,
}

impl RuntimeSourceSyncContext {
    pub(crate) async fn new(
        control_writer: ControlWriter,
        data_writer: DataWriter,
        suppress_payloads: bool,
    ) -> io::Result<(Self, tokio::net::tcp::OwnedReadHalf)> {
        let (offset_client, offset_reader) = RuntimeOffsetClient::connect().await?;
        let offset_client = Arc::new(offset_client);
        Ok((
            Self {
                control_writer,
                data_writer,
                offset_client,
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
        if self.suppress_payloads || tasks.is_empty() {
            return Ok(self.last_metrics());
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

        for chunk in chunk_source_payload_tasks(runtime_tasks) {
            block_on_handle(&self.control_writer.handle, async {
                self.data_writer
                    .write(&PluginDataFrame::SourcePayloadBatches { tasks: chunk })
                    .await
            })?;
        }

        Ok(self.last_metrics())
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
    ctx.submit_payload_tasks(vec![source_payload_task(batches)])
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
    ctx.submit_payload_tasks(tasks)
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
