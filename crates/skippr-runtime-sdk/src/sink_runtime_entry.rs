use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::io::{self, Cursor};
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};
use std::task::{Context, Poll};

use arrow::ipc::reader::StreamReader;
use arrow::record_batch::RecordBatch;
use arrow_schema::SchemaRef;
use skippr_core::helpers::logging::init_logging;
use skippr_core::plugins::{
    DataSink, HasSchemaSinkSpec, HasSinkSpec, RecordBatchChunk, SchemaSink, SchemaSyncRequest,
    SinkCallResult, SinkPreflightOutcome, SinkWriteOutcome,
};
#[cfg(test)]
use tokio::io::AsyncRead;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

#[cfg(test)]
use crate::protocol::FinishSink;
use crate::protocol::{
    CommitReceipt, CommitReceiptAuthority, HandshakeResponse, HostDataFrame, HostFrame,
    PluginFrame, PrepareAck, PrepareSink, PrepareSinkResult, RuntimeBinding, RuntimePluginKind,
    RuntimeRequestAck, RuntimeSchemaInstallRequest, RuntimeSchemaRefreshRequest,
    RuntimeSchemaState, RuntimeSchemaStateInstallRequest, RuntimeSessionHello,
    RuntimeSinkCapabilityDescriptor, RuntimeSinkError, RuntimeSinkInstallRequest,
    RuntimeSinkPayloadMode, SchemaDelta, SchemaRunRequest, SinkAck, SinkChunk, SinkRunRequest,
    RUNTIME_PROTOCOL_VERSION, SKIPPR_RUNTIME_CONTROL_ADDR_ENV, SKIPPR_RUNTIME_DATA_ADDR_ENV,
    SKIPPR_RUNTIME_SESSION_TOKEN_ENV,
};
use crate::sdk::decode_record_batch_stream;
use crate::sink_idempotency::ObjectWriteManifest;
use crate::wire::{read_frame_or_eof, write_frame};
use futures::Stream;
use skippr_core::plugins::GroupedBatchReader;
use skippr_core::sink_apply_identity::SINK_APPLY_ENVELOPE_V2;

const SINK_DATA_ROUTE_CAPACITY: usize = 1;

pub fn buffer_name_for_runtime_binding(binding: RuntimeBinding) -> String {
    match binding {
        RuntimeBinding::Primary => "output".to_string(),
        RuntimeBinding::Deadletter => "deadletters".to_string(),
    }
}

fn installed_schema_version(
    schema_state: &Option<RuntimeSchemaState>,
    namespace_versions: &BTreeMap<String, u64>,
    namespace: &str,
) -> u64 {
    if schema_state.is_none() {
        return 0;
    }
    namespace_versions.get(namespace).copied().unwrap_or(0)
}

fn schema_refresh_needed(
    schema_state: &Option<RuntimeSchemaState>,
    namespace_versions: &BTreeMap<String, u64>,
    required_schema_namespace: &str,
    required_schema_version: u64,
) -> bool {
    schema_state.is_none()
        || installed_schema_version(schema_state, namespace_versions, required_schema_namespace)
            < required_schema_version
}

#[derive(Clone)]
struct ControlWriter {
    writer: std::sync::Arc<tokio::sync::Mutex<OwnedWriteHalf>>,
}

impl ControlWriter {
    fn new(writer: OwnedWriteHalf) -> Self {
        Self {
            writer: std::sync::Arc::new(tokio::sync::Mutex::new(writer)),
        }
    }

    async fn write(&self, frame: &PluginFrame) -> io::Result<()> {
        let mut guard = self.writer.lock().await;
        write_frame(&mut *guard, frame).await
    }
}

#[derive(Default)]
struct SchemaApplyFences {
    global: Arc<tokio::sync::RwLock<()>>,
    namespaces: StdMutex<HashMap<String, Arc<tokio::sync::RwLock<()>>>>,
}

impl SchemaApplyFences {
    fn namespace(&self, namespace: &str) -> Arc<tokio::sync::RwLock<()>> {
        self.namespaces
            .lock()
            .expect("runtime schema namespace fences poisoned")
            .entry(namespace.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::RwLock::new(())))
            .clone()
    }

    async fn lock_delta_namespaces(
        &self,
        delta: &SchemaDelta,
    ) -> Vec<tokio::sync::OwnedRwLockWriteGuard<()>> {
        let mut guards = Vec::with_capacity(delta.namespaces.len());
        for namespace in delta.namespaces.keys() {
            guards.push(self.namespace(namespace).write_owned().await);
        }
        guards
    }
}

fn with_io_context(err: io::Error, context: impl AsRef<str>) -> io::Error {
    io::Error::new(err.kind(), format!("{}: {}", context.as_ref(), err))
}

async fn connect_runtime_channel(addr_env: &str) -> io::Result<TcpStream> {
    let addr = std::env::var(addr_env)
        .map_err(|_| io::Error::other(format!("missing runtime channel env {addr_env}")))?;
    let token = std::env::var(SKIPPR_RUNTIME_SESSION_TOKEN_ENV).map_err(|_| {
        io::Error::other(format!(
            "missing runtime session token env {}",
            SKIPPR_RUNTIME_SESSION_TOKEN_ENV
        ))
    })?;
    let mut stream = TcpStream::connect(&addr).await?;
    write_frame(
        &mut stream,
        &RuntimeSessionHello {
            protocol_version: RUNTIME_PROTOCOL_VERSION,
            token,
        },
    )
    .await?;
    Ok(stream)
}

async fn write_schema_refresh_required(
    writer: &ControlWriter,
    request_id: u64,
    namespace: &str,
    required_schema_version: u64,
    installed_schema_version: u64,
) -> io::Result<()> {
    writer
        .write(&PluginFrame::SchemaStateRefreshRequired(
            RuntimeSchemaRefreshRequest {
                request_id,
                namespace: namespace.to_string(),
                required_version: required_schema_version,
                installed_version: installed_schema_version,
            },
        ))
        .await
}

#[derive(Clone, Default)]
struct SinkDataRouter {
    routes: Arc<StdMutex<HashMap<u64, mpsc::Sender<HostDataFrame>>>>,
}

impl SinkDataRouter {
    fn register(&self, request_id: u64) -> io::Result<mpsc::Receiver<HostDataFrame>> {
        // Capacity one provides real socket-reader backpressure. Grouped
        // streaming retains one decoded look-ahead chunk to mark the final
        // chunk, so a request has at most: one look-ahead, one queued frame,
        // and one frame held by the shared demux while that queue is full.
        let (tx, rx) = mpsc::channel(SINK_DATA_ROUTE_CAPACITY);
        let mut routes = self
            .routes
            .lock()
            .expect("runtime sink data routes poisoned");
        if routes.contains_key(&request_id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("duplicate runtime sink request id {request_id}"),
            ));
        }
        routes.insert(request_id, tx);
        Ok(rx)
    }

    fn unregister(&self, request_id: u64) {
        self.routes
            .lock()
            .expect("runtime sink data routes poisoned")
            .remove(&request_id);
    }
}

async fn run_sink_data_demux(mut reader: OwnedReadHalf, router: SinkDataRouter) -> io::Result<()> {
    loop {
        let frame = read_frame_or_eof::<_, HostDataFrame>(&mut reader)
            .await
            .map_err(|err| with_io_context(err, "runtime sink data frame read failed"))?
            .ok_or_else(|| io::Error::other("runtime host closed sink data channel"))?;
        let request_id = match &frame {
            HostDataFrame::SinkChunk(chunk) => chunk.request_id,
            HostDataFrame::FinishSink(finish) => finish.request_id,
        };
        let sender = router
            .routes
            .lock()
            .expect("runtime sink data routes poisoned")
            .get(&request_id)
            .cloned()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("sink data frame for unknown request {request_id}"),
                )
            })?;
        sender.send(frame).await.map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("sink data frame for completed request {request_id}"),
            )
        })?;
    }
}

async fn read_sink_payload_channel(
    receiver: &mut mpsc::Receiver<HostDataFrame>,
    request_id: u64,
) -> io::Result<CollectedSinkPayload> {
    let mut chunks = Vec::new();
    let mut rows = 0u64;
    let mut bytes = 0u64;
    loop {
        let frame = receiver.recv().await.ok_or_else(|| {
            io::Error::other(format!(
                "runtime host closed sink data channel before FinishSink for request {request_id}"
            ))
        })?;
        match frame {
            HostDataFrame::SinkChunk(chunk) if chunk.chunk_index == chunks.len() as u32 => {
                debug_assert_eq!(chunk.request_id, request_id);
                chunk.validate_bound().map_err(io::Error::other)?;
                rows = rows.saturating_add(chunk.rows);
                bytes = bytes.saturating_add(chunk.arrow_stream_bytes.len() as u64);
                chunks.push(chunk);
            }
            HostDataFrame::SinkChunk(chunk) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "sink chunk mismatch for request {}: got chunk {}, expected chunk {}",
                        request_id,
                        chunk.chunk_index,
                        chunks.len()
                    ),
                ));
            }
            HostDataFrame::FinishSink(finish) => {
                debug_assert_eq!(finish.request_id, request_id);
                if finish.chunks != chunks.len() as u32
                    || finish.rows != rows
                    || finish.bytes != bytes
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "FinishSink totals mismatch for request {}: chunks {}/{}, rows {}/{}, bytes {}/{}",
                            request_id,
                            chunks.len(),
                            finish.chunks,
                            rows,
                            finish.rows,
                            bytes,
                            finish.bytes
                        ),
                    ));
                }
                return Ok(CollectedSinkPayload {
                    chunks,
                    rows,
                    bytes,
                });
            }
        }
    }
}

#[derive(Debug)]
struct CollectedSinkPayload {
    chunks: Vec<SinkChunk>,
    rows: u64,
    bytes: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RuntimeGroupedPayloadProgress {
    chunks: u32,
    rows: u64,
    bytes: u64,
    finished: bool,
}

struct DecodedRuntimeSinkChunk {
    chunk_index: u32,
    row_offset: u64,
    rows: u64,
    memory_bytes: usize,
    batches: Vec<RecordBatch>,
}

fn decode_runtime_sink_chunk(
    chunk: SinkChunk,
    expected_request_id: u64,
    expected_chunk_index: u32,
    expected_row_offset: u64,
    expected_schema: Option<&SchemaRef>,
) -> io::Result<(SchemaRef, DecodedRuntimeSinkChunk, u64)> {
    if chunk.request_id != expected_request_id
        || chunk.chunk_index != expected_chunk_index
        || chunk.row_offset != expected_row_offset
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "grouped sink chunk mismatch for request {}: got request {} chunk {} row offset {}, expected chunk {} row offset {}",
                expected_request_id,
                chunk.request_id,
                chunk.chunk_index,
                chunk.row_offset,
                expected_chunk_index,
                expected_row_offset
            ),
        ));
    }
    chunk.validate_bound().map_err(io::Error::other)?;
    let wire_bytes = chunk.arrow_stream_bytes.len() as u64;
    let reader = StreamReader::try_new(Cursor::new(chunk.arrow_stream_bytes), None)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
    let schema = reader.schema();
    if expected_schema.is_some_and(|expected| expected.as_ref() != schema.as_ref()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "grouped sink chunk {} schema does not match the first chunk",
                chunk.chunk_index
            ),
        ));
    }
    let batches = reader
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
    let decoded_rows = batches
        .iter()
        .map(|batch| batch.num_rows() as u64)
        .sum::<u64>();
    if decoded_rows != chunk.rows {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "grouped sink chunk {} row total mismatch: frame={} decoded={}",
                chunk.chunk_index, chunk.rows, decoded_rows
            ),
        ));
    }
    let memory_bytes = batches
        .iter()
        .flat_map(|batch| batch.columns())
        .map(|column| column.get_array_memory_size())
        .sum();
    Ok((
        schema,
        DecodedRuntimeSinkChunk {
            chunk_index: chunk.chunk_index,
            row_offset: chunk.row_offset,
            rows: chunk.rows,
            memory_bytes,
            batches,
        },
        wire_bytes,
    ))
}

impl DecodedRuntimeSinkChunk {
    fn into_record_batch_chunk(self, final_chunk: bool) -> RecordBatchChunk {
        RecordBatchChunk {
            chunk_index: self.chunk_index as u64,
            row_offset: self.row_offset,
            final_chunk,
            rows: self.rows,
            bytes: self.memory_bytes,
            batches: self.batches,
        }
    }
}

struct RuntimeGroupedChunkStream {
    request_id: u64,
    schema: SchemaRef,
    receiver: mpsc::Receiver<HostDataFrame>,
    pending: Option<DecodedRuntimeSinkChunk>,
    progress: Arc<StdMutex<RuntimeGroupedPayloadProgress>>,
    terminal: bool,
}

impl RuntimeGroupedChunkStream {
    async fn start(
        mut receiver: mpsc::Receiver<HostDataFrame>,
        request_id: u64,
    ) -> io::Result<(
        SchemaRef,
        Pin<Box<dyn Stream<Item = io::Result<RecordBatchChunk>> + Send + 'static>>,
        Arc<StdMutex<RuntimeGroupedPayloadProgress>>,
    )> {
        let first = receiver.recv().await.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "runtime host closed grouped sink data channel before first chunk for request {request_id}"
                ),
            )
        })?;
        let HostDataFrame::SinkChunk(first) = first else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("grouped sink request {request_id} received FinishSink before first chunk"),
            ));
        };
        let (schema, first, wire_bytes) = decode_runtime_sink_chunk(first, request_id, 0, 0, None)?;
        let progress = Arc::new(StdMutex::new(RuntimeGroupedPayloadProgress {
            chunks: 1,
            rows: first.rows,
            bytes: wire_bytes,
            finished: false,
        }));
        let stream = Self {
            request_id,
            schema: Arc::clone(&schema),
            receiver,
            pending: Some(first),
            progress: Arc::clone(&progress),
            terminal: false,
        };
        Ok((schema, Box::pin(stream), progress))
    }

    fn fail(&mut self, err: io::Error) -> Poll<Option<io::Result<RecordBatchChunk>>> {
        self.terminal = true;
        self.pending = None;
        Poll::Ready(Some(Err(err)))
    }
}

impl Stream for RuntimeGroupedChunkStream {
    type Item = io::Result<RecordBatchChunk>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.terminal {
            return Poll::Ready(None);
        }
        let frame = match self.receiver.poll_recv(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Some(frame)) => frame,
            Poll::Ready(None) => {
                let request_id = self.request_id;
                return self.fail(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!(
                        "runtime host closed grouped sink data channel before FinishSink for request {}",
                        request_id
                    ),
                ));
            }
        };
        match frame {
            HostDataFrame::SinkChunk(chunk) => {
                let snapshot = *self
                    .progress
                    .lock()
                    .expect("runtime grouped sink progress poisoned");
                let (schema, next, wire_bytes) = match decode_runtime_sink_chunk(
                    chunk,
                    self.request_id,
                    snapshot.chunks,
                    snapshot.rows,
                    Some(&self.schema),
                ) {
                    Ok(decoded) => decoded,
                    Err(err) => return self.fail(err),
                };
                debug_assert_eq!(schema.as_ref(), self.schema.as_ref());
                {
                    let mut progress = self
                        .progress
                        .lock()
                        .expect("runtime grouped sink progress poisoned");
                    progress.chunks = progress.chunks.saturating_add(1);
                    progress.rows = progress.rows.saturating_add(next.rows);
                    progress.bytes = progress.bytes.saturating_add(wire_bytes);
                }
                let previous = self
                    .pending
                    .replace(next)
                    .expect("grouped sink stream must retain one pending chunk");
                Poll::Ready(Some(Ok(previous.into_record_batch_chunk(false))))
            }
            HostDataFrame::FinishSink(finish) => {
                let mut progress = self
                    .progress
                    .lock()
                    .expect("runtime grouped sink progress poisoned");
                if finish.request_id != self.request_id
                    || finish.chunks != progress.chunks
                    || finish.rows != progress.rows
                    || finish.bytes != progress.bytes
                {
                    drop(progress);
                    let request_id = self.request_id;
                    return self.fail(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "FinishSink totals mismatch for grouped request {}",
                            request_id
                        ),
                    ));
                }
                progress.finished = true;
                drop(progress);
                self.terminal = true;
                let final_chunk = self
                    .pending
                    .take()
                    .expect("grouped sink stream must retain its final chunk");
                Poll::Ready(Some(Ok(final_chunk.into_record_batch_chunk(true))))
            }
        }
    }
}

#[cfg(test)]
async fn read_sink_payload<R>(reader: &mut R, request_id: u64) -> io::Result<CollectedSinkPayload>
where
    R: AsyncRead + Unpin,
{
    let mut chunks = Vec::new();
    let mut rows = 0u64;
    let mut bytes = 0u64;
    loop {
        let frame = read_frame_or_eof::<_, HostDataFrame>(reader)
            .await
            .map_err(|err| {
                with_io_context(
                    err,
                    format!("runtime sink request {request_id} payload read failed"),
                )
            })?
            .ok_or_else(|| {
                io::Error::other(format!(
                    "runtime host closed sink data channel before FinishSink for request {request_id}"
                ))
            })?;
        match frame {
            HostDataFrame::SinkChunk(chunk)
                if chunk.request_id == request_id && chunk.chunk_index == chunks.len() as u32 =>
            {
                chunk.validate_bound().map_err(io::Error::other)?;
                rows = rows.saturating_add(chunk.rows);
                bytes = bytes.saturating_add(chunk.arrow_stream_bytes.len() as u64);
                chunks.push(chunk);
            }
            HostDataFrame::SinkChunk(chunk) => {
                return Err(io::Error::other(format!(
                    "sink chunk mismatch for request {}: got request {} chunk {}, expected chunk {}",
                    request_id,
                    chunk.request_id,
                    chunk.chunk_index,
                    chunks.len()
                )));
            }
            HostDataFrame::FinishSink(FinishSink {
                request_id: finish_request_id,
                chunks: expected_chunks,
                rows: expected_rows,
                bytes: expected_bytes,
            }) if finish_request_id == request_id => {
                if expected_chunks != chunks.len() as u32
                    || expected_rows != rows
                    || expected_bytes != bytes
                {
                    return Err(io::Error::other(format!(
                        "FinishSink totals mismatch for request {}: chunks {}/{}, rows {}/{}, bytes {}/{}",
                        request_id,
                        chunks.len(),
                        expected_chunks,
                        rows,
                        expected_rows,
                        bytes,
                        expected_bytes
                    )));
                }
                return Ok(CollectedSinkPayload {
                    chunks,
                    rows,
                    bytes,
                });
            }
            HostDataFrame::FinishSink(finish) => {
                return Err(io::Error::other(format!(
                    "FinishSink request id mismatch: expected {} got {}",
                    request_id, finish.request_id
                )));
            }
        }
    }
}

pub async fn run_runtime_data_sink_plugin<P, F, Fut>(
    expect_plugin_name: &'static str,
    handshake_display_name: &'static str,
    sink_capability: Option<RuntimeSinkCapabilityDescriptor>,
    supports_schema: bool,
    bin_name: &'static str,
    mut build: F,
) -> io::Result<()>
where
    P: DataSink + HasSinkSpec + Send + Sync + 'static,
    F: FnMut(RuntimeSinkInstallRequest) -> Fut,
    Fut: Future<Output = io::Result<P>>,
{
    let log_level =
        std::env::var("SKIPPR_RUNTIME_LOG_LEVEL").unwrap_or_else(|_| "warn".to_string());
    init_logging(Some(log_level));

    let control_stream = connect_runtime_channel(SKIPPR_RUNTIME_CONTROL_ADDR_ENV)
        .await
        .map_err(|err| with_io_context(err, "runtime sink control channel connect failed"))?;
    let data_stream = connect_runtime_channel(SKIPPR_RUNTIME_DATA_ADDR_ENV)
        .await
        .map_err(|err| with_io_context(err, "runtime sink data channel connect failed"))?;
    let (mut control_reader, control_writer_raw) = control_stream.into_split();
    let (data_reader, _data_writer) = data_stream.into_split();
    let control_writer = ControlWriter::new(control_writer_raw);

    let Some(handshake_frame) = read_frame_or_eof::<_, HostFrame>(&mut control_reader)
        .await
        .map_err(|err| with_io_context(err, "runtime sink handshake read failed"))?
    else {
        return Ok(());
    };
    let handshake = match handshake_frame {
        HostFrame::Handshake(handshake) => handshake,
        other => {
            control_writer
                .write(&PluginFrame::Error(format!(
                    "expected handshake as first frame, got {:?}",
                    other
                )))
                .await?;
            return Err(io::Error::other(format!(
                "{}: runtime sink did not receive a handshake",
                bin_name
            )));
        }
    };

    if handshake.protocol_version != RUNTIME_PROTOCOL_VERSION {
        control_writer
            .write(&PluginFrame::Error(format!(
                "protocol version mismatch: host={} child={}",
                handshake.protocol_version, RUNTIME_PROTOCOL_VERSION
            )))
            .await?;
        return Err(io::Error::other("runtime protocol version mismatch"));
    }

    let spec_capability = <P::Spec as skippr_core::plugins::SinkSpec>::CAPABILITY;
    let spec_descriptor = RuntimeSinkCapabilityDescriptor::from(&spec_capability);
    let adapter_session_limit = spec_descriptor.max_sessions_per_child.max(1);
    if let Some(ref declared) = sink_capability {
        if declared != &spec_descriptor {
            return Err(io::Error::other(format!(
                "{}: runtime sink capability argument does not match SinkSpec: arg={:?} spec={:?}",
                bin_name, declared, spec_descriptor
            )));
        }
    }

    control_writer
        .write(&PluginFrame::HandshakeAck(HandshakeResponse {
            protocol_version: RUNTIME_PROTOCOL_VERSION,
            kind: RuntimePluginKind::DataSink,
            plugin_name: handshake_display_name.to_string(),
            source_capability: None,
            sink_capability: Some(spec_descriptor),
            supports_schema,
        }))
        .await
        .map_err(|err| with_io_context(err, "runtime sink handshake ack write failed"))?;

    let session_capacity = handshake
        .sink_session_capacity
        .clamp(1, 64)
        .min(adapter_session_limit);
    let session_permits = Arc::new(tokio::sync::Semaphore::new(session_capacity));
    let apply_fences = Arc::new(SchemaApplyFences::default());
    let data_router = SinkDataRouter::default();
    let mut data_demux = tokio::spawn(run_sink_data_demux(data_reader, data_router.clone()));
    let mut sessions = tokio::task::JoinSet::new();
    let mut primary_plugin: Option<Arc<P>> = None;
    let mut deadletter_plugin: Option<Arc<P>> = None;
    let mut schema_state: Option<RuntimeSchemaState> = None;
    let mut schema_namespace_versions = BTreeMap::new();

    loop {
        let frame = tokio::select! {
            frame = read_frame_or_eof::<_, HostFrame>(&mut control_reader) => {
                match frame.map_err(|err| with_io_context(err, "runtime sink control frame read failed"))? {
                    Some(frame) => frame,
                    None => {
                        data_demux.abort();
                        sessions.shutdown().await;
                        return Ok(());
                    }
                }
            }
            data_result = &mut data_demux => {
                sessions.shutdown().await;
                return match data_result {
                    Ok(Ok(())) => Err(io::Error::other("runtime sink data demux exited unexpectedly")),
                    Ok(Err(err)) => Err(err),
                    Err(err) => Err(io::Error::other(format!("runtime sink data demux task failed: {err}"))),
                };
            }
            completed = sessions.join_next(), if !sessions.is_empty() => {
                if let Some(Err(err)) = completed {
                    data_demux.abort();
                    sessions.shutdown().await;
                    return Err(io::Error::other(format!("runtime sink session task failed: {err}")));
                }
                continue;
            }
        };
        match frame {
            HostFrame::InstallSink(request) => {
                let _fence = Arc::clone(&apply_fences.global).write_owned().await;
                install_sink(
                    expect_plugin_name,
                    request,
                    &mut primary_plugin,
                    &mut deadletter_plugin,
                    schema_state.as_ref(),
                    &mut build,
                )
                .await
                .map_err(|err| with_io_context(err, "runtime sink install failed"))?;
                control_writer
                    .write(&PluginFrame::Installed)
                    .await
                    .map_err(|err| with_io_context(err, "runtime sink install ack write failed"))?;
            }
            HostFrame::InstallSchemaState(request) => {
                let _fence = Arc::clone(&apply_fences.global).write_owned().await;
                apply_schema_state_to_sinks(
                    &request,
                    &mut primary_plugin,
                    &mut deadletter_plugin,
                    &mut schema_state,
                    &mut schema_namespace_versions,
                )
                .await
                .map_err(|err| with_io_context(err, "runtime sink schema state install failed"))?;
                control_writer
                    .write(&PluginFrame::Installed)
                    .await
                    .map_err(|err| {
                        with_io_context(err, "runtime sink schema state ack write failed")
                    })?;
            }
            HostFrame::InstallSchemaDelta(delta) => {
                let _namespace_fences = apply_fences.lock_delta_namespaces(&delta).await;
                apply_schema_delta_to_sinks(
                    &delta,
                    &mut primary_plugin,
                    &mut deadletter_plugin,
                    &mut schema_state,
                    &mut schema_namespace_versions,
                )
                .await
                .map_err(|err| with_io_context(err, "runtime sink schema delta install failed"))?;
                control_writer
                    .write(&PluginFrame::Installed)
                    .await
                    .map_err(|err| {
                        with_io_context(err, "runtime sink schema delta ack write failed")
                    })?;
            }
            HostFrame::PrepareSink(prepare) => {
                let request_id = prepare.request_id;
                if request_id != prepare.request.request_id {
                    control_writer
                        .write(&PluginFrame::SinkError(RuntimeSinkError {
                            request_id,
                            message: format!(
                                "PrepareSink request id mismatch: frame={} request={}",
                                request_id, prepare.request.request_id
                            ),
                        }))
                        .await?;
                    continue;
                }
                let permit = Arc::clone(&session_permits)
                    .acquire_owned()
                    .await
                    .map_err(|_| io::Error::other("runtime sink session limiter closed"))?;
                let global_apply_guard = Arc::clone(&apply_fences.global).read_owned().await;
                let namespace_apply_guard = apply_fences
                    .namespace(&prepare.request.required_schema_namespace)
                    .read_owned()
                    .await;
                if schema_refresh_needed(
                    &schema_state,
                    &schema_namespace_versions,
                    &prepare.request.required_schema_namespace,
                    prepare.request.required_schema_version,
                ) {
                    write_schema_refresh_required(
                        &control_writer,
                        request_id,
                        &prepare.request.required_schema_namespace,
                        prepare.request.required_schema_version,
                        installed_schema_version(
                            &schema_state,
                            &schema_namespace_versions,
                            &prepare.request.required_schema_namespace,
                        ),
                    )
                    .await
                    .map_err(|err| {
                        with_io_context(
                            err,
                            format!(
                                "runtime sink request {} schema refresh write failed",
                                request_id
                            ),
                        )
                    })?;
                    drop(namespace_apply_guard);
                    drop(global_apply_guard);
                    drop(permit);
                    continue;
                }
                let data_receiver = match data_router.register(request_id) {
                    Ok(receiver) => receiver,
                    Err(err) => {
                        control_writer
                            .write(&PluginFrame::SinkError(RuntimeSinkError {
                                request_id,
                                message: err.to_string(),
                            }))
                            .await?;
                        continue;
                    }
                };
                let writer = control_writer.clone();
                let router = data_router.clone();
                let primary = primary_plugin.clone();
                let deadletter = deadletter_plugin.clone();
                sessions.spawn(async move {
                    let result = run_multiplexed_sink_session(
                        prepare,
                        primary,
                        deadletter,
                        data_receiver,
                        writer.clone(),
                        global_apply_guard,
                        namespace_apply_guard,
                        permit,
                    )
                    .await;
                    router.unregister(request_id);
                    if let Err(err) = result {
                        let _ = writer
                            .write(&PluginFrame::SinkError(RuntimeSinkError {
                                request_id,
                                message: format!(
                                    "{}: runtime sink request {} failed: {}",
                                    bin_name, request_id, err
                                ),
                            }))
                            .await;
                    }
                });
            }
            HostFrame::Shutdown => {
                data_demux.abort();
                sessions.shutdown().await;
                return Ok(());
            }
            other => {
                control_writer
                    .write(&PluginFrame::Error(format!(
                        "unexpected sink frame after handshake: {:?}",
                        other
                    )))
                    .await?;
                return Err(io::Error::other("unexpected sink frame"));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_multiplexed_sink_session<P>(
    prepare: PrepareSink,
    primary_plugin: Option<Arc<P>>,
    deadletter_plugin: Option<Arc<P>>,
    mut data_receiver: mpsc::Receiver<HostDataFrame>,
    control_writer: ControlWriter,
    _global_apply_guard: tokio::sync::OwnedRwLockReadGuard<()>,
    _namespace_apply_guard: tokio::sync::OwnedRwLockReadGuard<()>,
    _permit: tokio::sync::OwnedSemaphorePermit,
) -> io::Result<()>
where
    P: DataSink + HasSinkSpec + Send + Sync,
{
    let request = &prepare.request;
    let ledger_manifest =
        match prepare_sink_request(&prepare, &primary_plugin, &deadletter_plugin).await {
            Ok(PreparedSink::AlreadyApplied {
                receipt,
                catalog_intents,
            }) => {
                control_writer
                    .write(&PluginFrame::PrepareAck(PrepareAck {
                        request_id: request.request_id,
                        result: PrepareSinkResult::AlreadyApplied {
                            receipt,
                            catalog_intents,
                        },
                    }))
                    .await?;
                return Ok(());
            }
            Ok(PreparedSink::Ready { ledger_manifest }) => {
                control_writer
                    .write(&PluginFrame::PrepareAck(PrepareAck {
                        request_id: request.request_id,
                        result: PrepareSinkResult::Ready,
                    }))
                    .await?;
                ledger_manifest
            }
            Err(err) => {
                control_writer
                    .write(&PluginFrame::PrepareAck(PrepareAck {
                        request_id: request.request_id,
                        result: PrepareSinkResult::Rejected {
                            reason: err.to_string(),
                        },
                    }))
                    .await?;
                return Ok(());
            }
        };
    let (mut call_result, payload_rows, payload_bytes) = match request.payload_mode {
        RuntimeSinkPayloadMode::FullStream => {
            // FullStream is one Arrow IPC stream split only for transport. Its
            // framing is reassembled before decode; compaction uses the bounded
            // GroupedChunks path below.
            let payload = read_sink_payload_channel(&mut data_receiver, request.request_id).await?;
            let arrow_stream_bytes = payload
                .chunks
                .iter()
                .flat_map(|chunk| chunk.arrow_stream_bytes.iter().copied())
                .collect();
            let result = run_sink_request(
                request.clone(),
                arrow_stream_bytes,
                &primary_plugin,
                &deadletter_plugin,
            )
            .await?;
            (result, payload.rows, payload.bytes)
        }
        RuntimeSinkPayloadMode::GroupedChunks => {
            let (result, progress) = run_grouped_sink_request(
                request.clone(),
                data_receiver,
                &primary_plugin,
                &deadletter_plugin,
            )
            .await?;
            (result, progress.rows, progress.bytes)
        }
    };
    if let Some(manifest) = ledger_manifest.as_ref() {
        if matches!(
            call_result.outcome,
            SinkWriteOutcome::Applied | SinkWriteOutcome::AlreadyApplied
        ) {
            write_local_idempotency_manifest(manifest)?;
        }
    }
    control_writer
        .write(&PluginFrame::SinkAck(SinkAck {
            request_id: request.request_id,
            outcome: call_result.outcome.clone(),
            receipt: CommitReceipt::from_envelope(
                &prepare.envelope,
                CommitReceiptAuthority::SinkWrite,
            ),
            stats: {
                if call_result.stats.rows.is_none() {
                    call_result.stats.rows = Some(payload_rows);
                }
                if call_result.stats.bytes.is_none() {
                    call_result.stats.bytes = Some(payload_bytes);
                }
                call_result.stats
            },
            catalog_intents: call_result.catalog_intents,
        }))
        .await
        .map_err(|err| {
            with_io_context(
                err,
                format!(
                    "runtime sink request {} ack write failed",
                    request.request_id
                ),
            )
        })
}

async fn install_sink<P, F, Fut>(
    expect_plugin_name: &'static str,
    request: RuntimeSinkInstallRequest,
    primary_plugin: &mut Option<Arc<P>>,
    deadletter_plugin: &mut Option<Arc<P>>,
    schema_state: Option<&RuntimeSchemaState>,
    build: &mut F,
) -> io::Result<()>
where
    P: DataSink + Send + Sync,
    F: FnMut(RuntimeSinkInstallRequest) -> Fut,
    Fut: Future<Output = io::Result<P>>,
{
    request
        .config
        .0
        .expect_plugin(expect_plugin_name)
        .map_err(io::Error::other)?;

    let plugin_slot = match request.binding {
        RuntimeBinding::Primary => primary_plugin,
        RuntimeBinding::Deadletter => deadletter_plugin,
    };
    let plugin = build(request).await?;
    if let Some(schema_state) = schema_state {
        plugin.install_schema_snapshot(schema_state).await?;
    }
    *plugin_slot = Some(Arc::new(plugin));
    Ok(())
}

async fn apply_schema_state_to_sinks<P>(
    request: &RuntimeSchemaStateInstallRequest,
    primary_plugin: &mut Option<Arc<P>>,
    deadletter_plugin: &mut Option<Arc<P>>,
    schema_state: &mut Option<RuntimeSchemaState>,
    schema_namespace_versions: &mut BTreeMap<String, u64>,
) -> io::Result<()>
where
    P: DataSink + Send + Sync,
{
    if let Some(plugin) = primary_plugin.as_ref() {
        plugin
            .install_schema_snapshot(&request.schema_state)
            .await?;
    }
    if let Some(plugin) = deadletter_plugin.as_ref() {
        plugin
            .install_schema_snapshot(&request.schema_state)
            .await?;
    }
    *schema_namespace_versions = request.schema_state.namespace_versions.clone();
    *schema_state = Some(request.schema_state.clone());
    Ok(())
}

async fn apply_schema_delta_to_sinks<P>(
    delta: &SchemaDelta,
    primary_plugin: &mut Option<Arc<P>>,
    deadletter_plugin: &mut Option<Arc<P>>,
    schema_state: &mut Option<RuntimeSchemaState>,
    schema_namespace_versions: &mut BTreeMap<String, u64>,
) -> io::Result<()>
where
    P: DataSink + Send + Sync,
{
    if schema_state.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "runtime sink received SchemaDelta before initial SchemaState",
        ));
    }
    let effective = SchemaDelta {
        version: delta.version,
        namespaces: delta
            .namespaces
            .iter()
            .filter(|(namespace, entry)| {
                schema_namespace_versions
                    .get(*namespace)
                    .is_none_or(|installed| entry.version > *installed)
            })
            .map(|(namespace, entry)| (namespace.clone(), entry.clone()))
            .collect(),
    };
    if effective.namespaces.is_empty() {
        return Ok(());
    }
    if let Some(plugin) = primary_plugin.as_ref() {
        plugin.install_schema_delta(&effective).await?;
    }
    if let Some(plugin) = deadletter_plugin.as_ref() {
        plugin.install_schema_delta(&effective).await?;
    }
    let state = schema_state.as_mut().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "runtime sink received SchemaDelta before initial SchemaState",
        )
    })?;
    for (namespace, entry) in &effective.namespaces {
        state
            .namespaces
            .insert(namespace.clone(), entry.metadata.clone());
        state
            .namespace_versions
            .insert(namespace.clone(), entry.version);
        schema_namespace_versions.insert(namespace.clone(), entry.version);
    }
    state.version = state.version.max(delta.version);
    Ok(())
}

enum PreparedSink {
    Ready {
        ledger_manifest: Option<ObjectWriteManifest>,
    },
    AlreadyApplied {
        receipt: CommitReceipt,
        catalog_intents: Vec<crate::protocol::CatalogIntent>,
    },
}

fn sink_write_context(request: &SinkRunRequest) -> skippr_core::plugins::SinkWriteContext<'_> {
    skippr_core::plugins::SinkWriteContext {
        filename: request.filename.clone(),
        compaction_id: request.compaction_id.clone(),
        idempotency_key: request.idempotency_key.clone(),
        wal_refs: request.wal_refs.clone(),
        write_semantics: request.write_semantics,
        schema_fingerprint: request.schema_fingerprint.clone(),
        cdc_ctx: request.cdc_ctx.as_ref(),
        source_contract: request.source_contract.as_ref(),
    }
}

async fn prepare_sink_request<P>(
    prepare: &PrepareSink,
    primary_plugin: &Option<Arc<P>>,
    deadletter_plugin: &Option<Arc<P>>,
) -> io::Result<PreparedSink>
where
    P: DataSink + HasSinkSpec + Send + Sync,
{
    let request = &prepare.request;
    if prepare.request_id != request.request_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "PrepareSink request id mismatch: frame={} request={}",
                prepare.request_id, request.request_id
            ),
        ));
    }
    if prepare.envelope.version != SINK_APPLY_ENVELOPE_V2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unsupported sink apply envelope version {}",
                prepare.envelope.version
            ),
        ));
    }
    let expected_envelope = skippr_core::sink_apply_identity::SinkApplyEnvelopeV2::new(
        request.compaction_id.clone(),
        request.idempotency_key.clone(),
        &request.wal_refs,
        request.cdc_ctx.is_some(),
        request
            .source_contract
            .as_ref()
            .map(|contract| contract.write_policy)
            .unwrap_or_default(),
        request.write_semantics,
        (!request.schema_fingerprint.is_empty()).then(|| request.schema_fingerprint.clone()),
        Some(request.required_schema_version),
        request.source_contract.clone(),
    );
    if prepare.envelope != expected_envelope {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "PrepareSink envelope does not match SinkRunRequest",
        ));
    }
    let plugin = match request.binding {
        RuntimeBinding::Primary => primary_plugin,
        RuntimeBinding::Deadletter => deadletter_plugin,
    }
    .as_ref()
    .ok_or_else(|| io::Error::other("runtime sink binding was not installed"))?;
    let ctx = sink_write_context(request);
    ctx.validate_grouped::<<P::Spec as skippr_core::plugins::SinkSpec>::WriteSupport>()
        .map_err(|err| io::Error::new(io::ErrorKind::Unsupported, err))?;
    let replay_safe =
        <<P::Spec as skippr_core::plugins::SinkSpec>::WriteSupport as skippr_core::plugins::SinkWriteSupport>::CAN_RETURN_ALREADY_APPLIED;
    let ledger_manifest = if ctx.is_grouped() && replay_safe {
        Some(ObjectWriteManifest::from_context(
            ctx.compaction_id.clone(),
            ctx.idempotency_key.clone(),
            ctx.schema_fingerprint.clone(),
            &ctx.wal_refs,
        ))
    } else {
        None
    };
    // Probe local state during Prepare so a mismatch is detected before payload
    // consumption. A matching entry is only a cache hint; the sink preflight
    // remains the authority for AlreadyApplied.
    if let Some(manifest) = ledger_manifest.as_ref() {
        let _cache_matches = local_idempotency_manifest_matches(manifest)?;
    }
    let preflight = plugin.preflight_result(ctx).await?;
    match preflight.outcome {
        SinkPreflightOutcome::Ready => Ok(PreparedSink::Ready { ledger_manifest }),
        SinkPreflightOutcome::AlreadyApplied { authority } => {
            if !replay_safe {
                return Err(io::Error::other(
                    "runtime sink preflight returned AlreadyApplied without replay-safe capability",
                ));
            }
            if authority.trim().is_empty() {
                return Err(io::Error::other(
                    "runtime sink preflight returned an empty receipt authority",
                ));
            }
            if let Some(manifest) = ledger_manifest.as_ref() {
                write_local_idempotency_manifest(manifest)?;
            }
            Ok(PreparedSink::AlreadyApplied {
                receipt: CommitReceipt::from_envelope(
                    &prepare.envelope,
                    CommitReceiptAuthority::AuthoritativePreflight { authority },
                ),
                catalog_intents: preflight.catalog_intents,
            })
        }
    }
}

async fn run_sink_request<P>(
    request: SinkRunRequest,
    arrow_stream_bytes: Vec<u8>,
    primary_plugin: &Option<Arc<P>>,
    deadletter_plugin: &Option<Arc<P>>,
) -> io::Result<SinkCallResult>
where
    P: DataSink + HasSinkSpec + Send + Sync,
{
    let plugin_slot = match request.binding {
        RuntimeBinding::Primary => primary_plugin,
        RuntimeBinding::Deadletter => deadletter_plugin,
    };
    let plugin = plugin_slot
        .as_ref()
        .ok_or_else(|| io::Error::other("runtime sink binding was not installed"))?;
    let stream = decode_record_batch_stream(arrow_stream_bytes)?;
    let SinkRunRequest {
        filename,
        compaction_id,
        idempotency_key,
        wal_refs,
        write_semantics,
        schema_fingerprint,
        cdc_ctx,
        source_contract,
        ..
    } = request;
    let ctx = skippr_core::plugins::SinkWriteContext {
        filename,
        compaction_id,
        idempotency_key,
        wal_refs,
        write_semantics,
        schema_fingerprint,
        cdc_ctx: cdc_ctx.as_ref(),
        source_contract: source_contract.as_ref(),
    };
    ctx.validate_grouped::<<P::Spec as skippr_core::plugins::SinkSpec>::WriteSupport>()
        .map_err(|err| io::Error::new(io::ErrorKind::Unsupported, err))?;
    let is_grouped = ctx.is_grouped();
    let replay_safe =
        <<P::Spec as skippr_core::plugins::SinkSpec>::WriteSupport as skippr_core::plugins::SinkWriteSupport>::CAN_RETURN_ALREADY_APPLIED;
    let result = plugin.sync_with_context_call_result(stream, ctx).await?;
    if is_grouped && result.outcome == SinkWriteOutcome::AlreadyApplied && !replay_safe {
        return Err(io::Error::other(
            "runtime sink returned AlreadyApplied without declaring idempotent replay support",
        ));
    }
    Ok(result)
}

async fn run_grouped_sink_request<P>(
    request: SinkRunRequest,
    receiver: mpsc::Receiver<HostDataFrame>,
    primary_plugin: &Option<Arc<P>>,
    deadletter_plugin: &Option<Arc<P>>,
) -> io::Result<(SinkCallResult, RuntimeGroupedPayloadProgress)>
where
    P: DataSink + HasSinkSpec + Send + Sync,
{
    let request_id = request.request_id;
    let plugin_slot = match request.binding {
        RuntimeBinding::Primary => primary_plugin,
        RuntimeBinding::Deadletter => deadletter_plugin,
    };
    let plugin = plugin_slot
        .as_ref()
        .ok_or_else(|| io::Error::other("runtime sink binding was not installed"))?;
    let SinkRunRequest {
        filename,
        compaction_id,
        idempotency_key,
        wal_refs,
        write_semantics,
        schema_fingerprint,
        cdc_ctx,
        source_contract,
        ..
    } = request;
    let ctx = skippr_core::plugins::SinkWriteContext {
        filename,
        compaction_id,
        idempotency_key,
        wal_refs,
        write_semantics,
        schema_fingerprint,
        cdc_ctx: cdc_ctx.as_ref(),
        source_contract: source_contract.as_ref(),
    };
    ctx.validate_grouped::<<P::Spec as skippr_core::plugins::SinkSpec>::WriteSupport>()
        .map_err(|err| io::Error::new(io::ErrorKind::Unsupported, err))?;
    let grouped_ctx = skippr_core::plugins::GroupedSinkWriteContext::try_from(ctx)
        .map_err(|err| io::Error::new(io::ErrorKind::Unsupported, err))?;
    let replay_safe =
        <<P::Spec as skippr_core::plugins::SinkSpec>::WriteSupport as skippr_core::plugins::SinkWriteSupport>::CAN_RETURN_ALREADY_APPLIED;

    let (schema, chunk_stream, progress) =
        RuntimeGroupedChunkStream::start(receiver, request_id).await?;
    let reader = GroupedBatchReader::from_chunk_stream(
        schema,
        grouped_ctx.grouping_key.clone(),
        chunk_stream,
    );
    let result = plugin.sync_grouped_call_result(reader, grouped_ctx).await?;
    let progress = *progress
        .lock()
        .expect("runtime grouped sink progress poisoned");
    if !progress.finished {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "runtime grouped sink request {} returned before consuming FinishSink",
                request_id
            ),
        ));
    }
    if result.outcome == SinkWriteOutcome::AlreadyApplied && !replay_safe {
        return Err(io::Error::other(
            "runtime sink returned AlreadyApplied without declaring idempotent replay support",
        ));
    }

    Ok((result, progress))
}

fn local_idempotency_manifest_path(
    manifest: &ObjectWriteManifest,
) -> io::Result<std::path::PathBuf> {
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| ".".to_string());
    if manifest.idempotency_key.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "grouped sink idempotency ledger requires a non-empty key",
        ));
    }
    let safe_key = manifest
        .idempotency_key
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    Ok(std::path::Path::new(&data_dir)
        .join("segment_buffer")
        .join("sink_idempotency")
        .join(format!("{safe_key}.json")))
}

fn local_idempotency_manifest_matches(manifest: &ObjectWriteManifest) -> io::Result<bool> {
    let path = local_idempotency_manifest_path(manifest)?;
    if !path.exists() {
        return Ok(false);
    }
    let bytes = std::fs::read(&path)?;
    let existing = ObjectWriteManifest::from_json_bytes(&bytes)?;
    if existing.matches_manifest(manifest) {
        return Ok(true);
    }
    Ok(false)
}

fn write_local_idempotency_manifest(manifest: &ObjectWriteManifest) -> io::Result<()> {
    let path = local_idempotency_manifest_path(manifest)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, manifest.to_json_bytes()?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

pub async fn run_runtime_schema_sink_plugin<P, F, Fut>(
    expect_plugin_name: &'static str,
    handshake_display_name: &'static str,
    bin_name: &'static str,
    mut build: F,
) -> io::Result<()>
where
    P: SchemaSink + HasSchemaSinkSpec + Send + Sync,
    F: FnMut(RuntimeSchemaInstallRequest) -> Fut,
    Fut: Future<Output = io::Result<P>>,
{
    let log_level =
        std::env::var("SKIPPR_RUNTIME_LOG_LEVEL").unwrap_or_else(|_| "warn".to_string());
    init_logging(Some(log_level));

    let control_stream = connect_runtime_channel(SKIPPR_RUNTIME_CONTROL_ADDR_ENV).await?;
    let data_stream = connect_runtime_channel(SKIPPR_RUNTIME_DATA_ADDR_ENV).await?;
    let (mut control_reader, control_writer_raw) = control_stream.into_split();
    let (_data_reader, _data_writer) = data_stream.into_split();
    let control_writer = ControlWriter::new(control_writer_raw);

    let Some(handshake_frame) = read_frame_or_eof::<_, HostFrame>(&mut control_reader).await?
    else {
        return Ok(());
    };
    let handshake = match handshake_frame {
        HostFrame::Handshake(handshake) => handshake,
        other => {
            control_writer
                .write(&PluginFrame::Error(format!(
                    "expected handshake as first frame, got {:?}",
                    other
                )))
                .await?;
            return Err(io::Error::other(format!(
                "{}: runtime schema sink did not receive a handshake",
                bin_name
            )));
        }
    };

    if handshake.protocol_version != RUNTIME_PROTOCOL_VERSION {
        control_writer
            .write(&PluginFrame::Error(format!(
                "protocol version mismatch: host={} child={}",
                handshake.protocol_version, RUNTIME_PROTOCOL_VERSION
            )))
            .await?;
        return Err(io::Error::other("runtime protocol version mismatch"));
    }
    if <P::Spec as skippr_core::plugins::SchemaSinkSpec>::NAME != handshake_display_name {
        return Err(io::Error::other(format!(
            "{}: runtime schema sink display name '{}' does not match SchemaSinkSpec '{}'",
            bin_name,
            handshake_display_name,
            <P::Spec as skippr_core::plugins::SchemaSinkSpec>::NAME
        )));
    }

    control_writer
        .write(&PluginFrame::HandshakeAck(HandshakeResponse {
            protocol_version: RUNTIME_PROTOCOL_VERSION,
            kind: RuntimePluginKind::SchemaSink,
            plugin_name: handshake_display_name.to_string(),
            source_capability: None,
            sink_capability: None,
            supports_schema: true,
        }))
        .await?;

    let mut primary_plugin: Option<P> = None;
    let mut deadletter_plugin: Option<P> = None;
    let mut schema_state: Option<RuntimeSchemaState> = None;
    let mut schema_namespace_versions = BTreeMap::new();

    loop {
        let Some(frame) = read_frame_or_eof::<_, HostFrame>(&mut control_reader).await? else {
            return Ok(());
        };
        match frame {
            HostFrame::InstallSchema(request) => {
                install_schema_sink(
                    expect_plugin_name,
                    request,
                    &mut primary_plugin,
                    &mut deadletter_plugin,
                    schema_state.as_ref(),
                    &mut build,
                )
                .await?;
                control_writer.write(&PluginFrame::Installed).await?;
            }
            HostFrame::InstallSchemaState(request) => {
                apply_schema_state_to_schema_sinks(
                    &request,
                    &mut primary_plugin,
                    &mut deadletter_plugin,
                    &mut schema_state,
                    &mut schema_namespace_versions,
                )
                .await?;
                control_writer.write(&PluginFrame::Installed).await?;
            }
            HostFrame::InstallSchemaDelta(delta) => {
                apply_schema_delta_to_schema_sinks(
                    &delta,
                    &mut primary_plugin,
                    &mut deadletter_plugin,
                    &mut schema_state,
                    &mut schema_namespace_versions,
                )
                .await?;
                control_writer.write(&PluginFrame::Installed).await?;
            }
            HostFrame::RunSchema(request) => {
                if schema_refresh_needed(
                    &schema_state,
                    &schema_namespace_versions,
                    &request.namespace,
                    request.required_schema_version,
                ) {
                    write_schema_refresh_required(
                        &control_writer,
                        request.request_id,
                        &request.namespace,
                        request.required_schema_version,
                        installed_schema_version(
                            &schema_state,
                            &schema_namespace_versions,
                            &request.namespace,
                        ),
                    )
                    .await?;
                    continue;
                }
                run_schema_request(
                    request.clone(),
                    &schema_state,
                    &primary_plugin,
                    &deadletter_plugin,
                )
                .await?;
                control_writer
                    .write(&PluginFrame::SchemaAck(RuntimeRequestAck {
                        request_id: request.request_id,
                    }))
                    .await?;
            }
            HostFrame::Shutdown => return Ok(()),
            other => {
                control_writer
                    .write(&PluginFrame::Error(format!(
                        "unexpected schema frame after handshake: {:?}",
                        other
                    )))
                    .await?;
                return Err(io::Error::other("unexpected schema frame"));
            }
        }
    }
}

async fn install_schema_sink<P, F, Fut>(
    expect_plugin_name: &'static str,
    request: RuntimeSchemaInstallRequest,
    primary_plugin: &mut Option<P>,
    deadletter_plugin: &mut Option<P>,
    schema_state: Option<&RuntimeSchemaState>,
    build: &mut F,
) -> io::Result<()>
where
    P: SchemaSink + HasSchemaSinkSpec + Send + Sync,
    F: FnMut(RuntimeSchemaInstallRequest) -> Fut,
    Fut: Future<Output = io::Result<P>>,
{
    request
        .config
        .0
        .expect_plugin(expect_plugin_name)
        .map_err(io::Error::other)?;
    let plugin_slot = match request.binding {
        RuntimeBinding::Primary => primary_plugin,
        RuntimeBinding::Deadletter => deadletter_plugin,
    };
    let plugin = build(request).await?;
    if let Some(schema_state) = schema_state {
        plugin
            .install_schema_state(schema_state.version, &schema_state.namespaces)
            .await?;
    }
    *plugin_slot = Some(plugin);
    Ok(())
}

async fn apply_schema_state_to_schema_sinks<P>(
    request: &RuntimeSchemaStateInstallRequest,
    primary_plugin: &mut Option<P>,
    deadletter_plugin: &mut Option<P>,
    schema_state: &mut Option<RuntimeSchemaState>,
    schema_namespace_versions: &mut BTreeMap<String, u64>,
) -> io::Result<()>
where
    P: SchemaSink + HasSchemaSinkSpec + Send + Sync,
{
    if let Some(plugin) = primary_plugin.as_ref() {
        plugin
            .install_schema_state(
                request.schema_state.version,
                &request.schema_state.namespaces,
            )
            .await?;
    }
    if let Some(plugin) = deadletter_plugin.as_ref() {
        plugin
            .install_schema_state(
                request.schema_state.version,
                &request.schema_state.namespaces,
            )
            .await?;
    }
    *schema_namespace_versions = request.schema_state.namespace_versions.clone();
    *schema_state = Some(request.schema_state.clone());
    Ok(())
}

async fn apply_schema_delta_to_schema_sinks<P>(
    delta: &SchemaDelta,
    primary_plugin: &mut Option<P>,
    deadletter_plugin: &mut Option<P>,
    schema_state: &mut Option<RuntimeSchemaState>,
    schema_namespace_versions: &mut BTreeMap<String, u64>,
) -> io::Result<()>
where
    P: SchemaSink + HasSchemaSinkSpec + Send + Sync,
{
    let state = schema_state.as_mut().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "runtime schema sink received SchemaDelta before initial SchemaState",
        )
    })?;
    for (namespace, entry) in &delta.namespaces {
        if schema_namespace_versions
            .get(namespace)
            .is_some_and(|installed| *installed >= entry.version)
        {
            continue;
        }
        state
            .namespaces
            .insert(namespace.clone(), entry.metadata.clone());
        state
            .namespace_versions
            .insert(namespace.clone(), entry.version);
        schema_namespace_versions.insert(namespace.clone(), entry.version);
    }
    state.version = state.version.max(delta.version);
    let _ = (primary_plugin, deadletter_plugin);
    Ok(())
}

async fn run_schema_request<P>(
    request: SchemaRunRequest,
    schema_state: &Option<RuntimeSchemaState>,
    primary_plugin: &Option<P>,
    deadletter_plugin: &Option<P>,
) -> io::Result<()>
where
    P: SchemaSink + HasSchemaSinkSpec + Send + Sync,
{
    let plugin_slot = match request.binding {
        RuntimeBinding::Primary => primary_plugin,
        RuntimeBinding::Deadletter => deadletter_plugin,
    };
    let plugin = plugin_slot
        .as_ref()
        .ok_or_else(|| io::Error::other("runtime schema binding was not installed"))?;
    let metadata = schema_state
        .as_ref()
        .and_then(|state| state.namespaces.get(&request.namespace))
        .ok_or_else(|| {
            io::Error::other(format!(
                "runtime schema state does not contain namespace '{}'",
                request.namespace
            ))
        })?;
    plugin
        .sync_schema_request(
            SchemaSyncRequest {
                namespace: &request.namespace,
                compaction_id: &request.compaction_id,
                source_contract: request.source_contract.as_ref(),
            },
            metadata,
        )
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::write_frame;
    use arrow::array::Int64Array;
    use arrow::datatypes::{DataType, Field, Schema};
    use skippr_core::plugins::cdc::{sink_capabilities, SinkCapability};
    use skippr_core::plugins::{
        DeterministicObjectOverwrite, GroupedBatchReader, HasSinkSpec, SinkSpec,
    };
    use std::sync::{Mutex as StdMutex, OnceLock};

    struct TestSink;
    struct TestSinkSpec;

    impl SinkSpec for TestSinkSpec {
        const NAME: &'static str = "Test";
        const CAPABILITY: SinkCapability = sink_capabilities::FILE;
        type WriteSupport = DeterministicObjectOverwrite;
    }

    impl HasSinkSpec for TestSink {
        type Spec = TestSinkSpec;
    }

    #[async_trait::async_trait]
    impl DataSink for TestSink {
        async fn sync(
            &self,
            _stream: datafusion::execution::SendableRecordBatchStream,
            _filename: String,
            _cdc_ctx: Option<&skippr_core::plugins::cdc::SyncContext>,
        ) -> io::Result<()> {
            Ok(())
        }

        async fn sync_grouped(
            &self,
            _reader: GroupedBatchReader,
            _ctx: skippr_core::plugins::GroupedSinkWriteContext<'_>,
        ) -> io::Result<SinkWriteOutcome> {
            Ok(SinkWriteOutcome::Applied)
        }

        fn capability(&self) -> &'static SinkCapability {
            &sink_capabilities::FILE
        }
    }

    struct StreamingTestSink {
        first_chunk_seen: Arc<tokio::sync::Notify>,
        chunks: Arc<StdMutex<Vec<(u64, u64, bool)>>>,
    }

    impl HasSinkSpec for StreamingTestSink {
        type Spec = TestSinkSpec;
    }

    #[async_trait::async_trait]
    impl DataSink for StreamingTestSink {
        async fn sync(
            &self,
            _stream: datafusion::execution::SendableRecordBatchStream,
            _filename: String,
            _cdc_ctx: Option<&skippr_core::plugins::cdc::SyncContext>,
        ) -> io::Result<()> {
            Ok(())
        }

        async fn sync_grouped(
            &self,
            mut reader: GroupedBatchReader,
            _ctx: skippr_core::plugins::GroupedSinkWriteContext<'_>,
        ) -> io::Result<SinkWriteOutcome> {
            while let Some(chunk) = reader.next_chunk().await? {
                let mut chunks = self.chunks.lock().unwrap();
                chunks.push((chunk.chunk_index, chunk.row_offset, chunk.final_chunk));
                let first = chunks.len() == 1;
                drop(chunks);
                if first {
                    self.first_chunk_seen.notify_one();
                }
            }
            Ok(SinkWriteOutcome::Applied)
        }

        fn capability(&self) -> &'static SinkCapability {
            &sink_capabilities::FILE
        }
    }

    fn grouped_test_request(request_id: u64) -> SinkRunRequest {
        SinkRunRequest {
            request_id,
            compaction_id: format!("c{request_id}"),
            idempotency_key: format!("k{request_id}"),
            wal_refs: vec![crate::protocol::RuntimeWalPartRef {
                segment_id: "seg-1".into(),
                source: "local".into(),
                start: 0,
                len: 10,
                sink_ref: "primary".into(),
                namespace: "events".into(),
                partition: "day=2026-07-30".into(),
                time: None,
                schema_fingerprint: "schema".into(),
                cdc_meta_hash: None,
            }],
            write_semantics:
                skippr_core::buffer::compaction_transaction::SinkWriteSemantics::IdempotentAtLeastOnce,
            schema_fingerprint: "schema".into(),
            binding: RuntimeBinding::Primary,
            required_schema_namespace: "events".into(),
            required_schema_version: 3,
            filename: "namespace=events".into(),
            cdc_ctx: None,
            source_contract: None,
            payload_mode: RuntimeSinkPayloadMode::GroupedChunks,
        }
    }

    fn encoded_test_chunk(request_id: u64, chunk_index: u32, row_offset: u64) -> SinkChunk {
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        let batch = RecordBatch::try_new(
            schema,
            vec![Arc::new(Int64Array::from(vec![chunk_index as i64]))],
        )
        .unwrap();
        SinkChunk {
            request_id,
            chunk_index,
            row_offset,
            rows: 1,
            arrow_stream_bytes: crate::sdk::encode_record_batches(&[batch]).unwrap(),
        }
    }

    #[tokio::test]
    async fn ready_payload_requires_explicit_finish_sink() {
        let (mut writer, mut reader) = tokio::io::duplex(4096);
        write_frame(
            &mut writer,
            &HostDataFrame::SinkChunk(SinkChunk {
                request_id: 7,
                chunk_index: 0,
                row_offset: 0,
                rows: 1,
                arrow_stream_bytes: vec![1, 2, 3],
            }),
        )
        .await
        .unwrap();
        drop(writer);

        let err = read_sink_payload(&mut reader, 7).await.unwrap_err();
        assert!(err.to_string().contains("before FinishSink"));
    }

    #[tokio::test]
    async fn grouped_chunks_stream_before_finish_with_bounded_backpressure() {
        let request_id = 41;
        let first_chunk_seen = Arc::new(tokio::sync::Notify::new());
        let chunks = Arc::new(StdMutex::new(Vec::new()));
        let sink = Arc::new(StreamingTestSink {
            first_chunk_seen: Arc::clone(&first_chunk_seen),
            chunks: Arc::clone(&chunks),
        });
        let (sender, receiver) = mpsc::channel(SINK_DATA_ROUTE_CAPACITY);
        let request = grouped_test_request(request_id);
        let run = tokio::spawn(async move {
            run_grouped_sink_request(request, receiver, &Some(sink), &None).await
        });

        sender
            .send(HostDataFrame::SinkChunk(encoded_test_chunk(
                request_id, 0, 0,
            )))
            .await
            .unwrap();
        sender
            .send(HostDataFrame::SinkChunk(encoded_test_chunk(
                request_id, 1, 1,
            )))
            .await
            .unwrap();
        let seen = first_chunk_seen.notified();
        tokio::time::timeout(std::time::Duration::from_secs(1), seen)
            .await
            .expect("sink should consume the first transport chunk before FinishSink");
        assert!(!run.is_finished());

        let finish = HostDataFrame::FinishSink(crate::protocol::FinishSink {
            request_id,
            chunks: 2,
            rows: 2,
            bytes: encoded_test_chunk(request_id, 0, 0)
                .arrow_stream_bytes
                .len() as u64
                + encoded_test_chunk(request_id, 1, 1)
                    .arrow_stream_bytes
                    .len() as u64,
        });
        sender.send(finish).await.unwrap();
        let (outcome, progress) = run.await.unwrap().unwrap();
        assert_eq!(outcome.outcome, SinkWriteOutcome::Applied);
        assert_eq!(
            progress,
            RuntimeGroupedPayloadProgress {
                chunks: 2,
                rows: 2,
                bytes: progress.bytes,
                finished: true,
            }
        );
        assert_eq!(*chunks.lock().unwrap(), vec![(0, 0, false), (1, 1, true)]);

        let (sender, mut receiver) = mpsc::channel(SINK_DATA_ROUTE_CAPACITY);
        sender
            .send(HostDataFrame::SinkChunk(encoded_test_chunk(
                request_id, 0, 0,
            )))
            .await
            .unwrap();
        let blocked = sender.send(HostDataFrame::SinkChunk(encoded_test_chunk(
            request_id, 1, 1,
        )));
        tokio::pin!(blocked);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), &mut blocked)
                .await
                .is_err(),
            "a second queued 128 MiB-capable frame must backpressure the demux"
        );
        let _ = receiver.recv().await.unwrap();
        blocked.await.unwrap();
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn matching_local_ledger_is_cache_not_already_applied_authority() {
        static ENV_LOCK: OnceLock<StdMutex<()>> = OnceLock::new();
        let _guard = ENV_LOCK.get_or_init(|| StdMutex::new(())).lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let old_data_dir = std::env::var("DATA_DIR").ok();
        std::env::set_var("DATA_DIR", temp.path());
        let wal_refs = vec![crate::protocol::RuntimeWalPartRef {
            segment_id: "seg-1".into(),
            source: "local".into(),
            start: 0,
            len: 10,
            sink_ref: "primary".into(),
            namespace: "events".into(),
            partition: "day=2026-07-30".into(),
            time: None,
            schema_fingerprint: "schema".into(),
            cdc_meta_hash: None,
        }];
        let request = SinkRunRequest {
            request_id: 9,
            compaction_id: "c9".into(),
            idempotency_key: "k9".into(),
            wal_refs: wal_refs.clone(),
            write_semantics:
                skippr_core::buffer::compaction_transaction::SinkWriteSemantics::IdempotentAtLeastOnce,
            schema_fingerprint: "schema".into(),
            binding: RuntimeBinding::Primary,
            required_schema_namespace: "events".into(),
            required_schema_version: 3,
            filename: "namespace=events".into(),
            cdc_ctx: None,
            source_contract: None,
            payload_mode: RuntimeSinkPayloadMode::GroupedChunks,
        };
        let manifest = ObjectWriteManifest::from_context("c9", "k9", "schema", &request.wal_refs);
        write_local_idempotency_manifest(&manifest).unwrap();
        let prepare = PrepareSink {
            request_id: 9,
            envelope: skippr_core::sink_apply_identity::SinkApplyEnvelopeV2::new(
                "c9",
                "k9",
                &wal_refs,
                false,
                Default::default(),
                request.write_semantics,
                Some("schema".into()),
                Some(3),
                None,
            ),
            request,
        };

        let result = prepare_sink_request(&prepare, &Some(Arc::new(TestSink)), &None)
            .await
            .unwrap();
        assert!(matches!(result, PreparedSink::Ready { .. }));

        if let Some(old_data_dir) = old_data_dir {
            std::env::set_var("DATA_DIR", old_data_dir);
        } else {
            std::env::remove_var("DATA_DIR");
        }
    }
}
