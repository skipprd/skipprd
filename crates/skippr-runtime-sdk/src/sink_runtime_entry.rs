use std::future::Future;
use std::io;

use skippr_core::helpers::logging::init_logging;
use skippr_core::plugins::{
    DataSink, HasSchemaSinkSpec, HasSinkSpec, SchemaSink, SchemaSyncRequest, SinkWriteOutcome,
};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;

use crate::protocol::{
    HandshakeResponse, HostDataFrame, HostFrame, PluginFrame, RuntimeBinding, RuntimePluginKind,
    RuntimeRequestAck, RuntimeSchemaInstallRequest, RuntimeSchemaRefreshRequest,
    RuntimeSchemaState, RuntimeSchemaStateInstallRequest, RuntimeSessionHello,
    RuntimeSinkCapabilityDescriptor, RuntimeSinkInstallRequest, RuntimeSinkWriteAck,
    RuntimeSinkWriteResult, SchemaRunRequest, SinkRunRequest, RUNTIME_PROTOCOL_VERSION,
    SKIPPR_RUNTIME_CONTROL_ADDR_ENV, SKIPPR_RUNTIME_DATA_ADDR_ENV,
    SKIPPR_RUNTIME_SESSION_TOKEN_ENV,
};
use crate::sdk::decode_record_batch_stream;
use crate::wire::{read_frame_or_eof, write_frame};

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
    required_schema_version: u64,
    installed_schema_version: u64,
) -> io::Result<()> {
    writer
        .write(&PluginFrame::SchemaStateRefreshRequired(
            RuntimeSchemaRefreshRequest {
                required_version: required_schema_version,
                installed_version: installed_schema_version,
            },
        ))
        .await
}

async fn read_sink_payload(reader: &mut OwnedReadHalf, request_id: u64) -> io::Result<Vec<u8>> {
    let frame = read_frame_or_eof::<_, HostDataFrame>(reader)
        .await
        .map_err(|err| {
            with_io_context(
                err,
                format!("runtime sink request {request_id} payload read failed"),
            )
        })?;
    match frame {
        Some(HostDataFrame::SinkPayload(payload)) if payload.request_id == request_id => {
            Ok(payload.arrow_stream_bytes)
        }
        Some(HostDataFrame::SinkPayload(payload)) => Err(io::Error::other(format!(
            "sink payload request id mismatch: expected {} got {}",
            request_id, payload.request_id
        ))),
        None => Err(io::Error::other(format!(
            "runtime host closed sink data channel before request {request_id} payload"
        ))),
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
    P: DataSink + HasSinkSpec + Send + Sync,
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
    let (mut data_reader, _data_writer) = data_stream.into_split();
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

    let mut primary_plugin: Option<P> = None;
    let mut deadletter_plugin: Option<P> = None;
    let mut schema_state: Option<RuntimeSchemaState> = None;

    loop {
        let Some(frame) = read_frame_or_eof::<_, HostFrame>(&mut control_reader)
            .await
            .map_err(|err| with_io_context(err, "runtime sink control frame read failed"))?
        else {
            return Ok(());
        };
        match frame {
            HostFrame::InstallSink(request) => {
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
            HostFrame::RunSink(request) => {
                if schema_refresh_needed(&schema_state, request.required_schema_version) {
                    // The host already sent the payload for this request on the data channel.
                    // Drain it before asking for a schema refresh so retries stay aligned.
                    let _ = read_sink_payload(&mut data_reader, request.request_id).await?;
                    write_schema_refresh_required(
                        &control_writer,
                        request.required_schema_version,
                        installed_schema_version(&schema_state),
                    )
                    .await
                    .map_err(|err| {
                        with_io_context(
                            err,
                            format!(
                                "runtime sink request {} schema refresh write failed",
                                request.request_id
                            ),
                        )
                    })?;
                    continue;
                }
                let arrow_stream_bytes =
                    read_sink_payload(&mut data_reader, request.request_id).await?;
                let outcome = match run_sink_request(
                    request.clone(),
                    arrow_stream_bytes,
                    &primary_plugin,
                    &deadletter_plugin,
                )
                .await
                {
                    Ok(outcome) => outcome,
                    Err(err) => {
                        let message = format!(
                            "{}: runtime sink request {} failed: {}",
                            bin_name, request.request_id, err
                        );
                        let _ = control_writer
                            .write(&PluginFrame::Error(message.clone()))
                            .await;
                        return Err(io::Error::other(message));
                    }
                };
                control_writer
                    .write(&PluginFrame::SinkWriteAck(RuntimeSinkWriteAck {
                        request_id: request.request_id,
                        result: match outcome {
                            SinkWriteOutcome::Applied => RuntimeSinkWriteResult::Applied,
                            SinkWriteOutcome::AlreadyApplied => {
                                RuntimeSinkWriteResult::AlreadyApplied
                            }
                        },
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
                    })?;
            }
            HostFrame::Shutdown => return Ok(()),
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

async fn install_sink<P, F, Fut>(
    expect_plugin_name: &'static str,
    request: RuntimeSinkInstallRequest,
    primary_plugin: &mut Option<P>,
    deadletter_plugin: &mut Option<P>,
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
    *plugin_slot = Some(plugin);
    Ok(())
}

async fn apply_schema_state_to_sinks<P>(
    request: &RuntimeSchemaStateInstallRequest,
    primary_plugin: &mut Option<P>,
    deadletter_plugin: &mut Option<P>,
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

async fn run_sink_request<P>(
    request: SinkRunRequest,
    arrow_stream_bytes: Vec<u8>,
    primary_plugin: &Option<P>,
    deadletter_plugin: &Option<P>,
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
    plugin.sync_with_context_result(stream, ctx).await
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
            HostFrame::RunSchema(request) => {
                if schema_refresh_needed(&schema_state, request.required_schema_version) {
                    write_schema_refresh_required(
                        &control_writer,
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
