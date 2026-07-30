use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::sync::{Arc, Mutex as StdMutex};

use skippr_core::helpers::logging::init_logging;
use skippr_core::plugins::{
    DataSink, HasSchemaSinkSpec, HasSinkSpec, SchemaSink, SchemaSyncRequest, SinkPreflightOutcome,
    SinkWriteOutcome,
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
    SinkWriteStats, RUNTIME_PROTOCOL_VERSION, SKIPPR_RUNTIME_CONTROL_ADDR_ENV,
    SKIPPR_RUNTIME_DATA_ADDR_ENV, SKIPPR_RUNTIME_SESSION_TOKEN_ENV,
};
use crate::sdk::{decode_record_batch_stream, record_batches_to_stream};
use crate::sink_idempotency::ObjectWriteManifest;
use crate::wire::{read_frame_or_eof, write_frame};
use futures::StreamExt;
use skippr_core::plugins::{GroupedBatchReader, GroupedBatchReaderConfig};
use skippr_core::sink_apply_identity::SINK_APPLY_ENVELOPE_V2;

pub fn buffer_name_for_runtime_binding(binding: RuntimeBinding) -> String {
    match binding {
        RuntimeBinding::Primary => "output".to_string(),
        RuntimeBinding::Deadletter => "deadletters".to_string(),
    }
}

fn installed_schema_version(schema_state: &Option<RuntimeSchemaState>) -> u64 {
    schema_state
        .as_ref()
        .map(|state| state.version)
        .unwrap_or(0)
}

fn schema_refresh_needed(
    schema_state: &Option<RuntimeSchemaState>,
    required_schema_version: u64,
) -> bool {
    schema_state.is_none() || installed_schema_version(schema_state) < required_schema_version
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
    required_schema_version: u64,
    installed_schema_version: u64,
) -> io::Result<()> {
    writer
        .write(&PluginFrame::SchemaStateRefreshRequired(
            RuntimeSchemaRefreshRequest {
                request_id,
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
        let (tx, rx) = mpsc::channel(8);
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

    let session_capacity = handshake.sink_session_capacity.clamp(1, 64);
    let session_permits = Arc::new(tokio::sync::Semaphore::new(session_capacity));
    let apply_fence = Arc::new(tokio::sync::RwLock::new(()));
    let data_router = SinkDataRouter::default();
    let mut data_demux = tokio::spawn(run_sink_data_demux(data_reader, data_router.clone()));
    let mut sessions = tokio::task::JoinSet::new();
    let mut primary_plugin: Option<Arc<P>> = None;
    let mut deadletter_plugin: Option<Arc<P>> = None;
    let mut schema_state: Option<RuntimeSchemaState> = None;

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
                let _fence = apply_fence.write().await;
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
                let _fence = apply_fence.write().await;
                apply_schema_state_to_sinks(
                    &request,
                    &mut primary_plugin,
                    &mut deadletter_plugin,
                    &mut schema_state,
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
                let _fence = apply_fence.write().await;
                apply_schema_delta_to_sinks(
                    &delta,
                    &mut primary_plugin,
                    &mut deadletter_plugin,
                    &mut schema_state,
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
                let apply_guard = Arc::clone(&apply_fence).read_owned().await;
                if schema_refresh_needed(&schema_state, prepare.request.required_schema_version) {
                    write_schema_refresh_required(
                        &control_writer,
                        request_id,
                        prepare.request.required_schema_version,
                        installed_schema_version(&schema_state),
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
                    drop(apply_guard);
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
                        apply_guard,
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

async fn run_multiplexed_sink_session<P>(
    prepare: PrepareSink,
    primary_plugin: Option<Arc<P>>,
    deadletter_plugin: Option<Arc<P>>,
    mut data_receiver: mpsc::Receiver<HostDataFrame>,
    control_writer: ControlWriter,
    _apply_guard: tokio::sync::OwnedRwLockReadGuard<()>,
    _permit: tokio::sync::OwnedSemaphorePermit,
) -> io::Result<()>
where
    P: DataSink + HasSinkSpec + Send + Sync,
{
    let request = &prepare.request;
    let ledger_manifest =
        match prepare_sink_request(&prepare, &primary_plugin, &deadletter_plugin).await {
            Ok(PreparedSink::AlreadyApplied(receipt)) => {
                control_writer
                    .write(&PluginFrame::PrepareAck(PrepareAck {
                        request_id: request.request_id,
                        result: PrepareSinkResult::AlreadyApplied(receipt),
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
    let payload = read_sink_payload_channel(&mut data_receiver, request.request_id).await?;
    let outcome = match request.payload_mode {
        RuntimeSinkPayloadMode::FullStream => {
            let arrow_stream_bytes = payload
                .chunks
                .iter()
                .flat_map(|chunk| chunk.arrow_stream_bytes.iter().copied())
                .collect();
            run_sink_request(
                request.clone(),
                arrow_stream_bytes,
                &primary_plugin,
                &deadletter_plugin,
            )
            .await?
        }
        RuntimeSinkPayloadMode::GroupedChunks => {
            run_grouped_sink_request(
                request.clone(),
                &payload.chunks,
                &primary_plugin,
                &deadletter_plugin,
            )
            .await?
        }
    };
    if let Some(manifest) = ledger_manifest.as_ref() {
        if matches!(
            outcome,
            SinkWriteOutcome::Applied | SinkWriteOutcome::AlreadyApplied
        ) {
            write_local_idempotency_manifest(manifest)?;
        }
    }
    control_writer
        .write(&PluginFrame::SinkAck(SinkAck {
            request_id: request.request_id,
            outcome,
            receipt: CommitReceipt::from_envelope(
                &prepare.envelope,
                CommitReceiptAuthority::SinkWrite,
            ),
            stats: SinkWriteStats {
                rows: Some(payload.rows),
                bytes: Some(payload.bytes),
                ..SinkWriteStats::default()
            },
            // Glue outbox migration owns production of catalog intents.
            catalog_intents: Vec::new(),
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
        plugin
            .install_schema_state(schema_state.version, &schema_state.namespaces)
            .await?;
    }
    *plugin_slot = Some(Arc::new(plugin));
    Ok(())
}

async fn apply_schema_state_to_sinks<P>(
    request: &RuntimeSchemaStateInstallRequest,
    primary_plugin: &mut Option<Arc<P>>,
    deadletter_plugin: &mut Option<Arc<P>>,
    schema_state: &mut Option<RuntimeSchemaState>,
) -> io::Result<()>
where
    P: DataSink + Send + Sync,
{
    *schema_state = Some(request.schema_state.clone());
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
    Ok(())
}

async fn apply_schema_delta_to_sinks<P>(
    delta: &SchemaDelta,
    primary_plugin: &mut Option<Arc<P>>,
    deadletter_plugin: &mut Option<Arc<P>>,
    schema_state: &mut Option<RuntimeSchemaState>,
) -> io::Result<()>
where
    P: DataSink + Send + Sync,
{
    let state = schema_state.as_mut().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "runtime sink received SchemaDelta before initial SchemaState",
        )
    })?;
    if delta.version <= state.version {
        return Ok(());
    }
    for (namespace, entry) in &delta.namespaces {
        state
            .namespaces
            .insert(namespace.clone(), entry.metadata.clone());
    }
    state.version = delta.version;
    if let Some(plugin) = primary_plugin.as_ref() {
        plugin
            .install_schema_state(state.version, &state.namespaces)
            .await?;
    }
    if let Some(plugin) = deadletter_plugin.as_ref() {
        plugin
            .install_schema_state(state.version, &state.namespaces)
            .await?;
    }
    Ok(())
}

enum PreparedSink {
    Ready {
        ledger_manifest: Option<ObjectWriteManifest>,
    },
    AlreadyApplied(CommitReceipt),
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
    match plugin.preflight(ctx).await? {
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
            Ok(PreparedSink::AlreadyApplied(CommitReceipt::from_envelope(
                &prepare.envelope,
                CommitReceiptAuthority::AuthoritativePreflight { authority },
            )))
        }
    }
}

async fn run_sink_request<P>(
    request: SinkRunRequest,
    arrow_stream_bytes: Vec<u8>,
    primary_plugin: &Option<Arc<P>>,
    deadletter_plugin: &Option<Arc<P>>,
) -> io::Result<SinkWriteOutcome>
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
    let outcome = plugin.sync_with_context_result(stream, ctx).await?;
    if is_grouped && outcome == SinkWriteOutcome::AlreadyApplied && !replay_safe {
        return Err(io::Error::other(
            "runtime sink returned AlreadyApplied without declaring idempotent replay support",
        ));
    }
    Ok(outcome)
}

async fn run_grouped_sink_request<P>(
    request: SinkRunRequest,
    chunks: &[SinkChunk],
    primary_plugin: &Option<Arc<P>>,
    deadletter_plugin: &Option<Arc<P>>,
) -> io::Result<SinkWriteOutcome>
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

    let mut all_batches = Vec::new();
    let mut schema = None;
    for chunk in chunks {
        let mut stream = decode_record_batch_stream(chunk.arrow_stream_bytes.clone())?;
        if schema.is_none() {
            schema = Some(stream.schema());
        }
        while let Some(batch) = stream.next().await {
            all_batches.push(batch?);
        }
    }

    let schema = schema.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "grouped sink request contained no schema",
        )
    })?;
    let batch_stream = record_batches_to_stream(schema.clone(), all_batches);
    let reader = GroupedBatchReader::new(
        batch_stream,
        grouped_ctx.grouping_key.clone(),
        GroupedBatchReaderConfig::default(),
    );
    let outcome = plugin.sync_grouped(reader, grouped_ctx).await?;
    if outcome == SinkWriteOutcome::AlreadyApplied && !replay_safe {
        return Err(io::Error::other(
            "runtime sink returned AlreadyApplied without declaring idempotent replay support",
        ));
    }

    Ok(outcome)
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
                )
                .await?;
                control_writer.write(&PluginFrame::Installed).await?;
            }
            HostFrame::RunSchema(request) => {
                if schema_refresh_needed(&schema_state, request.required_schema_version) {
                    write_schema_refresh_required(
                        &control_writer,
                        request.request_id,
                        request.required_schema_version,
                        installed_schema_version(&schema_state),
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
) -> io::Result<()>
where
    P: SchemaSink + HasSchemaSinkSpec + Send + Sync,
{
    *schema_state = Some(request.schema_state.clone());
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
    Ok(())
}

async fn apply_schema_delta_to_schema_sinks<P>(
    delta: &SchemaDelta,
    primary_plugin: &mut Option<P>,
    deadletter_plugin: &mut Option<P>,
    schema_state: &mut Option<RuntimeSchemaState>,
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
    if delta.version <= state.version {
        return Ok(());
    }
    for (namespace, entry) in &delta.namespaces {
        state
            .namespaces
            .insert(namespace.clone(), entry.metadata.clone());
    }
    state.version = delta.version;
    if let Some(plugin) = primary_plugin.as_ref() {
        plugin
            .install_schema_state(state.version, &state.namespaces)
            .await?;
    }
    if let Some(plugin) = deadletter_plugin.as_ref() {
        plugin
            .install_schema_state(state.version, &state.namespaces)
            .await?;
    }
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
