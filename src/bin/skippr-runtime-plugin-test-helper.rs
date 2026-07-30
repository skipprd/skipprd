use std::collections::HashMap;
use std::io;
use std::io::Write;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use arrow::array::{Int64Array, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use clap::Parser;
use datafusion::error::DataFusionError;
use datafusion::physical_plan::{RecordBatchStream, SendableRecordBatchStream};
use futures::{Stream, StreamExt};
use parquet::arrow::ArrowWriter;
use serde_json::json;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};

use skipprd::buffer::compaction_transaction::SinkWriteSemantics;
use skipprd::plugins::cdc;
use skipprd::plugins::SinkWriteOutcome;
use skipprd::runtime_plugins::protocol::{
    CommitReceipt, CommitReceiptAuthority, HandshakeResponse, HostDataFrame, HostFrame,
    PluginDataFrame, PluginFrame, PrepareAck, PrepareSinkResult, RuntimeCheckpointUpdate,
    RuntimePluginKind, RuntimeRequestAck, RuntimeSchemaInstallRequest, RuntimeSchemaRefreshRequest,
    RuntimeSchemaStateInstallRequest, RuntimeSessionHello, RuntimeSinkError,
    RuntimeSinkInstallRequest, RuntimeSourceSinkWrite, SinkAck, SinkWriteStats, SourceEvent,
    RUNTIME_PROTOCOL_VERSION, SKIPPR_RUNTIME_CONTROL_ADDR_ENV, SKIPPR_RUNTIME_DATA_ADDR_ENV,
    SKIPPR_RUNTIME_SESSION_TOKEN_ENV,
};
use skipprd::runtime_plugins::sdk::{decode_record_batch_stream, encode_record_batch_stream};
use skipprd::runtime_plugins::wire::{read_frame_or_eof, write_frame};

#[derive(Debug, Parser)]
struct TestHelperCli {
    #[arg(long)]
    kind: String,
    #[arg(long)]
    plugin_name: String,
    #[arg(long, default_value = "normal")]
    scenario: String,
    #[arg(long)]
    marker_path: Option<PathBuf>,
    #[arg(long, default_value_t = false)]
    supports_schema: bool,
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("skippr-runtime-plugin-test-helper: {}", err);
        std::process::exit(1);
    }
}

async fn run() -> io::Result<()> {
    let cli = TestHelperCli::parse();
    let kind = parse_kind(&cli.kind)?;
    let control_stream = connect_runtime_channel(SKIPPR_RUNTIME_CONTROL_ADDR_ENV).await?;
    let data_stream = connect_runtime_channel(SKIPPR_RUNTIME_DATA_ADDR_ENV).await?;
    let (mut control_reader, mut control_writer) = control_stream.into_split();
    let (mut data_reader, mut data_writer) = data_stream.into_split();

    let handshake = match read_frame_or_eof::<_, HostFrame>(&mut control_reader).await? {
        Some(HostFrame::Handshake(handshake)) => handshake,
        None => return Ok(()),
        other => {
            write_frame(
                &mut control_writer,
                &PluginFrame::Error(format!(
                    "expected handshake as first frame, got {:?}",
                    other
                )),
            )
            .await?;
            return Err(io::Error::other("test helper did not receive a handshake"));
        }
    };

    if handshake.protocol_version != RUNTIME_PROTOCOL_VERSION {
        return Err(io::Error::other("runtime protocol version mismatch"));
    }

    if cli.scenario == "handshake_error_frame" {
        write_frame(
            &mut control_writer,
            &PluginFrame::Error("simulated handshake setup failure".to_string()),
        )
        .await?;
        return Ok(());
    }

    let response = HandshakeResponse {
        protocol_version: RUNTIME_PROTOCOL_VERSION,
        kind,
        plugin_name: cli.plugin_name.clone(),
        source_capability: if kind == RuntimePluginKind::DataSource {
            cdc::source_capabilities::by_name(&cli.plugin_name).map(Into::into)
        } else {
            None
        },
        sink_capability: if kind == RuntimePluginKind::DataSink {
            cdc::sink_capabilities::by_name(&cli.plugin_name).map(Into::into)
        } else {
            None
        },
        supports_schema: cli.supports_schema,
    };
    write_frame(&mut control_writer, &PluginFrame::HandshakeAck(response)).await?;

    match kind {
        RuntimePluginKind::DataSink => {
            if cli.scenario.starts_with("multiplex_") {
                run_multiplex_sink_loop(
                    &cli,
                    handshake.sink_session_capacity,
                    control_reader,
                    control_writer,
                    data_reader,
                )
                .await
            } else {
                run_sink_loop(
                    &cli,
                    &mut control_reader,
                    &mut control_writer,
                    &mut data_reader,
                )
                .await
            }
        }
        RuntimePluginKind::SchemaSink => {
            run_schema_loop(&cli, &mut control_reader, &mut control_writer).await
        }
        RuntimePluginKind::DataSource => {
            run_source_loop(
                &cli,
                &mut control_reader,
                &mut control_writer,
                &mut data_writer,
            )
            .await
        }
    }
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

fn parse_kind(value: &str) -> io::Result<RuntimePluginKind> {
    match value {
        "data_source" | "DataSource" => Ok(RuntimePluginKind::DataSource),
        "data_sink" | "DataSink" => Ok(RuntimePluginKind::DataSink),
        "schema_sink" | "SchemaSink" => Ok(RuntimePluginKind::SchemaSink),
        _ => Err(io::Error::other(format!(
            "unsupported runtime plugin kind '{}'",
            value
        ))),
    }
}

#[derive(Default)]
struct MultiplexHelperState {
    active: usize,
    max_active: usize,
    run_count: usize,
    request_ids: Vec<u64>,
    data_request_ids: Vec<u64>,
    data_frame_request_ids: Vec<u64>,
    schema_install_active_counts: Vec<usize>,
}

fn write_multiplex_state(
    marker_path: Option<&PathBuf>,
    state: &MultiplexHelperState,
) -> io::Result<()> {
    let Some(marker_path) = marker_path else {
        return Ok(());
    };
    if let Some(parent) = marker_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        marker_path,
        serde_json::to_vec_pretty(&json!({
            "active": state.active,
            "max_active": state.max_active,
            "run_count": state.run_count,
            "request_ids": state.request_ids,
            "data_request_ids": state.data_request_ids,
            "data_frame_request_ids": state.data_frame_request_ids,
            "schema_install_active_counts": state.schema_install_active_counts,
        }))
        .map_err(io::Error::other)?,
    )
}

fn record_multiplex_process(marker_path: Option<&PathBuf>) -> io::Result<()> {
    let Some(marker_path) = marker_path else {
        return Ok(());
    };
    let process_path = marker_path.with_extension("processes");
    if let Some(parent) = process_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(process_path)?;
    writeln!(file, "{}", std::process::id())
}

async fn run_multiplex_sink_loop(
    cli: &TestHelperCli,
    session_capacity: usize,
    mut control_reader: OwnedReadHalf,
    control_writer: OwnedWriteHalf,
    mut data_reader: OwnedReadHalf,
) -> io::Result<()> {
    let writer = Arc::new(Mutex::new(control_writer));
    let marker_path = cli.marker_path.clone();
    let scenario = cli.scenario.clone();
    let state = Arc::new(Mutex::new(MultiplexHelperState::default()));
    let routes = Arc::new(std::sync::Mutex::new(HashMap::<
        u64,
        mpsc::Sender<HostDataFrame>,
    >::new()));
    let data_routes = Arc::clone(&routes);
    let data_state = Arc::clone(&state);
    let data_marker_path = marker_path.clone();
    let data_scenario = scenario.clone();
    let mut data_task = tokio::spawn(async move {
        loop {
            let frame = read_frame_or_eof::<_, HostDataFrame>(&mut data_reader)
                .await?
                .ok_or_else(|| io::Error::other("multiplex helper data channel closed"))?;
            let request_id = match &frame {
                HostDataFrame::SinkChunk(chunk) => chunk.request_id,
                HostDataFrame::FinishSink(finish) => finish.request_id,
            };
            {
                let mut state = data_state.lock().await;
                state.data_frame_request_ids.push(request_id);
                write_multiplex_state(data_marker_path.as_ref(), &state)?;
                if data_scenario == "multiplex_disconnect_all" {
                    let mut request_ids = state.data_frame_request_ids.clone();
                    request_ids.sort_unstable();
                    request_ids.dedup();
                    if request_ids.len() >= 2 {
                        std::process::exit(1);
                    }
                }
            }
            let sender = data_routes
                .lock()
                .expect("multiplex helper routes poisoned")
                .get(&request_id)
                .cloned()
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("multiplex helper got data for unknown request {request_id}"),
                    )
                })?;
            sender.send(frame).await.map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("multiplex helper route closed for request {request_id}"),
                )
            })?;
        }
    });
    let permits = Arc::new(tokio::sync::Semaphore::new(session_capacity.clamp(1, 64)));
    let mut sessions = tokio::task::JoinSet::new();

    loop {
        let frame = tokio::select! {
            frame = read_frame_or_eof::<_, HostFrame>(&mut control_reader) => {
                match frame? {
                    Some(frame) => frame,
                    None => {
                        data_task.abort();
                        sessions.shutdown().await;
                        return Ok(());
                    }
                }
            }
            result = &mut data_task => {
                sessions.shutdown().await;
                return match result {
                    Ok(Ok(())) => Err(io::Error::other("multiplex helper data task exited")),
                    Ok(Err(err)) => Err(err),
                    Err(err) => Err(io::Error::other(err.to_string())),
                };
            }
            completed = sessions.join_next(), if !sessions.is_empty() => {
                if let Some(Err(err)) = completed {
                    data_task.abort();
                    sessions.shutdown().await;
                    return Err(io::Error::other(err.to_string()));
                }
                continue;
            }
        };
        match frame {
            HostFrame::InstallSink(_) => {
                record_multiplex_process(marker_path.as_ref())?;
                let mut guard = writer.lock().await;
                write_frame(&mut *guard, &PluginFrame::Installed).await?;
            }
            HostFrame::InstallSchemaState(_) | HostFrame::InstallSchemaDelta(_) => {
                {
                    let mut state = state.lock().await;
                    let active = state.active;
                    state.schema_install_active_counts.push(active);
                    write_multiplex_state(marker_path.as_ref(), &state)?;
                }
                let mut guard = writer.lock().await;
                write_frame(&mut *guard, &PluginFrame::Installed).await?;
            }
            HostFrame::PrepareSink(prepare) => {
                let request_id = prepare.request_id;
                if scenario == "multiplex_bad_request_id" {
                    let mut guard = writer.lock().await;
                    write_frame(
                        &mut *guard,
                        &PluginFrame::PrepareAck(PrepareAck {
                            request_id: request_id.saturating_add(10_000),
                            result: PrepareSinkResult::Ready,
                        }),
                    )
                    .await?;
                    continue;
                }
                let permit = Arc::clone(&permits)
                    .acquire_owned()
                    .await
                    .map_err(|_| io::Error::other("multiplex helper permits closed"))?;
                let (sender, receiver) = mpsc::channel(8);
                let mut route_guard = routes.lock().expect("multiplex helper routes poisoned");
                if route_guard.contains_key(&request_id) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("duplicate multiplex helper request {request_id}"),
                    ));
                }
                route_guard.insert(request_id, sender);
                drop(route_guard);
                {
                    let mut state = state.lock().await;
                    state.active += 1;
                    state.max_active = state.max_active.max(state.active);
                    state.request_ids.push(request_id);
                    write_multiplex_state(marker_path.as_ref(), &state)?;
                }
                {
                    let mut guard = writer.lock().await;
                    write_frame(
                        &mut *guard,
                        &PluginFrame::PrepareAck(PrepareAck {
                            request_id,
                            result: PrepareSinkResult::Ready,
                        }),
                    )
                    .await?;
                }
                let writer = Arc::clone(&writer);
                let routes = Arc::clone(&routes);
                let state = Arc::clone(&state);
                let marker_path = marker_path.clone();
                let scenario = scenario.clone();
                sessions.spawn(async move {
                    let payload = read_multiplex_helper_payload(receiver, request_id).await;
                    routes
                        .lock()
                        .expect("multiplex helper routes poisoned")
                        .remove(&request_id);
                    let payload = match payload {
                        Ok(payload) => payload,
                        Err(err) => {
                            let mut guard = writer.lock().await;
                            let _ = write_frame(
                                &mut *guard,
                                &PluginFrame::SinkError(RuntimeSinkError {
                                    request_id,
                                    message: err.to_string(),
                                }),
                            )
                            .await;
                            return;
                        }
                    };
                    if scenario == "multiplex_disconnect_after_payload_once" {
                        let crash_marker = marker_path
                            .as_ref()
                            .map(|path| path.with_extension("crash"));
                        if should_crash_once(crash_marker.as_ref()).unwrap_or(false) {
                            std::process::exit(1);
                        }
                    }
                    if scenario == "multiplex_delay" || scenario == "multiplex_schema_fence" {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    if scenario == "multiplex_process_hold" {
                        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    }
                    {
                        let mut state = state.lock().await;
                        state.active = state.active.saturating_sub(1);
                        state.run_count += 1;
                        state.data_request_ids.push(request_id);
                        let _ = write_multiplex_state(marker_path.as_ref(), &state);
                    }
                    let response = if scenario == "multiplex_one_failure"
                        && prepare.request.filename.contains("fail")
                    {
                        PluginFrame::SinkError(RuntimeSinkError {
                            request_id,
                            message: "simulated session failure".to_string(),
                        })
                    } else {
                        let mut receipt = CommitReceipt::from_envelope(
                            &prepare.envelope,
                            CommitReceiptAuthority::SinkWrite,
                        );
                        if scenario == "multiplex_receipt_mismatch" {
                            receipt.idempotency_key.push_str("-wrong");
                        }
                        PluginFrame::SinkAck(SinkAck {
                            request_id,
                            outcome: SinkWriteOutcome::Applied,
                            receipt,
                            stats: SinkWriteStats {
                                rows: Some(payload.rows),
                                bytes: Some(payload.byte_count),
                                ..SinkWriteStats::default()
                            },
                            catalog_intents: Vec::new(),
                        })
                    };
                    let mut guard = writer.lock().await;
                    let _ = write_frame(&mut *guard, &response).await;
                    drop(permit);
                });
            }
            HostFrame::Shutdown => {
                data_task.abort();
                sessions.shutdown().await;
                return Ok(());
            }
            other => {
                return Err(io::Error::other(format!(
                    "unexpected multiplex helper frame: {other:?}"
                )));
            }
        }
    }
}

async fn read_multiplex_helper_payload(
    mut receiver: mpsc::Receiver<HostDataFrame>,
    request_id: u64,
) -> io::Result<HelperSinkPayload> {
    let mut bytes = Vec::new();
    let mut rows = 0u64;
    let mut expected_index = 0u32;
    loop {
        match receiver.recv().await {
            Some(HostDataFrame::SinkChunk(chunk)) if chunk.chunk_index == expected_index => {
                if chunk.request_id != request_id {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "multiplex helper chunk request mismatch",
                    ));
                }
                chunk.validate_bound().map_err(io::Error::other)?;
                rows = rows.saturating_add(chunk.rows);
                bytes.extend_from_slice(&chunk.arrow_stream_bytes);
                expected_index = expected_index.saturating_add(1);
            }
            Some(HostDataFrame::FinishSink(finish)) => {
                if finish.request_id != request_id
                    || finish.chunks != expected_index
                    || finish.rows != rows
                    || finish.bytes != bytes.len() as u64
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "multiplex helper FinishSink mismatch",
                    ));
                }
                return Ok(HelperSinkPayload {
                    byte_count: bytes.len() as u64,
                    bytes,
                    rows,
                });
            }
            Some(other) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unexpected multiplex helper data frame: {other:?}"),
                ));
            }
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "multiplex helper data route closed",
                ));
            }
        }
    }
}

async fn run_sink_loop(
    cli: &TestHelperCli,
    control_reader: &mut OwnedReadHalf,
    control_writer: &mut OwnedWriteHalf,
    data_reader: &mut OwnedReadHalf,
) -> io::Result<()> {
    let mut state = SinkHelperState::default();
    loop {
        let Some(frame) = read_frame_or_eof::<_, HostFrame>(control_reader).await? else {
            return Ok(());
        };
        match frame {
            HostFrame::InstallSink(request) => {
                state.install_request = Some(request);
                write_sink_state_snapshot(cli, &state)?;
                write_frame(control_writer, &PluginFrame::Installed).await?;
            }
            HostFrame::InstallSchemaState(request) => {
                if cli.scenario == "disconnect_on_schema_install_once"
                    && should_crash_once(cli.marker_path.as_ref())?
                {
                    return Ok(());
                }
                record_schema_state_install(cli, request.schema_state.version)?;
                state.schema_state = Some(request);
                write_sink_state_snapshot(cli, &state)?;
                write_frame(control_writer, &PluginFrame::Installed).await?;
            }
            HostFrame::InstallSchemaDelta(delta) => {
                if let Some(schema_state) = state.schema_state.as_mut() {
                    schema_state.schema_state.version =
                        schema_state.schema_state.version.max(delta.version);
                    for (namespace, entry) in delta.namespaces {
                        if schema_state
                            .schema_state
                            .namespace_versions
                            .get(&namespace)
                            .is_some_and(|installed| *installed >= entry.version)
                        {
                            continue;
                        }
                        schema_state
                            .schema_state
                            .namespaces
                            .insert(namespace.clone(), entry.metadata);
                        schema_state
                            .schema_state
                            .namespace_versions
                            .insert(namespace, entry.version);
                    }
                }
                write_sink_state_snapshot(cli, &state)?;
                write_frame(control_writer, &PluginFrame::Installed).await?;
            }
            HostFrame::PrepareSink(prepare) => {
                let request = &prepare.request;
                if cli.scenario == "crash_once" && should_crash_once(cli.marker_path.as_ref())? {
                    std::process::exit(1);
                }
                if cli.scenario == "disconnect_once" && should_crash_once(cli.marker_path.as_ref())?
                {
                    return Ok(());
                }
                if cli.scenario == "stale_prepare_once"
                    && should_crash_once(cli.marker_path.as_ref())?
                {
                    write_frame(control_writer, &PluginFrame::Installed).await?;
                    continue;
                }
                state.request_ids.push(request.request_id);
                state.compaction_ids.push(request.compaction_id.clone());
                if cli.scenario == "refresh_once" && !state.refresh_requested_once {
                    state.refresh_requested_once = true;
                    write_sink_state_snapshot(cli, &state)?;
                    write_frame(
                        control_writer,
                        &PluginFrame::SchemaStateRefreshRequired(RuntimeSchemaRefreshRequest {
                            request_id: request.request_id,
                            namespace: request.required_schema_namespace.clone(),
                            required_version: request.required_schema_version,
                            installed_version: state
                                .schema_state
                                .as_ref()
                                .and_then(|schema| {
                                    schema
                                        .schema_state
                                        .namespace_versions
                                        .get(&request.required_schema_namespace)
                                        .copied()
                                })
                                .unwrap_or(0),
                        }),
                    )
                    .await?;
                    continue;
                }
                if cli.scenario == "reject_prepare" {
                    write_sink_state_snapshot(cli, &state)?;
                    write_frame(
                        control_writer,
                        &PluginFrame::PrepareAck(PrepareAck {
                            request_id: request.request_id,
                            result: PrepareSinkResult::Rejected {
                                reason: "simulated prepare rejection".to_string(),
                            },
                        }),
                    )
                    .await?;
                    continue;
                }
                if cli.scenario == "already_applied" {
                    write_frame(
                        control_writer,
                        &PluginFrame::PrepareAck(PrepareAck {
                            request_id: request.request_id,
                            result: PrepareSinkResult::AlreadyApplied {
                                receipt: CommitReceipt::from_envelope(
                                    &prepare.envelope,
                                    CommitReceiptAuthority::AuthoritativePreflight {
                                        authority: "test-helper".to_string(),
                                    },
                                ),
                                catalog_intents: Vec::new(),
                            },
                        }),
                    )
                    .await?;
                    write_sink_state_snapshot(cli, &state)?;
                    continue;
                }
                write_frame(
                    control_writer,
                    &PluginFrame::PrepareAck(PrepareAck {
                        request_id: request.request_id,
                        result: PrepareSinkResult::Ready,
                    }),
                )
                .await?;
                let payload = read_sink_payload(data_reader, request.request_id).await?;
                let arrow_stream_bytes = payload.bytes;
                if matches!(sink_scenario(cli), SinkScenario::WriteOutputParquet) {
                    let context = state
                        .install_request
                        .as_ref()
                        .map(|install| &install.context)
                        .ok_or_else(|| io::Error::other("sink run received before install"))?;
                    write_sink_request_to_output(arrow_stream_bytes, context).await?;
                }
                state.run_count += 1;
                state.payload_bytes = state.payload_bytes.saturating_add(payload.byte_count);
                write_sink_state_snapshot(cli, &state)?;
                write_frame(
                    control_writer,
                    &PluginFrame::SinkAck(SinkAck {
                        request_id: request.request_id,
                        outcome: SinkWriteOutcome::Applied,
                        receipt: CommitReceipt::from_envelope(
                            &prepare.envelope,
                            CommitReceiptAuthority::SinkWrite,
                        ),
                        stats: SinkWriteStats {
                            rows: Some(payload.rows),
                            bytes: Some(payload.byte_count),
                            ..SinkWriteStats::default()
                        },
                        catalog_intents: Vec::new(),
                    }),
                )
                .await?;
            }
            HostFrame::Shutdown => return Ok(()),
            other => {
                write_frame(
                    control_writer,
                    &PluginFrame::Error(format!(
                        "unexpected sink frame after handshake: {:?}",
                        other
                    )),
                )
                .await?;
                return Err(io::Error::other("unexpected sink frame"));
            }
        }
    }
}

#[derive(Default)]
struct SinkHelperState {
    install_request: Option<RuntimeSinkInstallRequest>,
    schema_state: Option<RuntimeSchemaStateInstallRequest>,
    refresh_requested_once: bool,
    run_count: usize,
    request_ids: Vec<u64>,
    compaction_ids: Vec<String>,
    payload_bytes: u64,
}

#[derive(Clone, Copy)]
enum SinkScenario {
    AckOnly,
    WriteOutputParquet,
}

fn sink_scenario(cli: &TestHelperCli) -> SinkScenario {
    match cli.scenario.as_str() {
        "write_output_parquet" => SinkScenario::WriteOutputParquet,
        _ => SinkScenario::AckOnly,
    }
}

async fn run_schema_loop(
    cli: &TestHelperCli,
    control_reader: &mut OwnedReadHalf,
    control_writer: &mut OwnedWriteHalf,
) -> io::Result<()> {
    let mut state = SchemaHelperState::default();
    loop {
        let Some(frame) = read_frame_or_eof::<_, HostFrame>(control_reader).await? else {
            return Ok(());
        };
        match frame {
            HostFrame::InstallSchema(request) => {
                state.install_request = Some(request);
                write_schema_state_snapshot(cli, &state)?;
                write_frame(control_writer, &PluginFrame::Installed).await?;
            }
            HostFrame::InstallSchemaState(request) => {
                if cli.scenario == "disconnect_on_schema_install_once"
                    && should_crash_once(cli.marker_path.as_ref())?
                {
                    return Ok(());
                }
                record_schema_state_install(cli, request.schema_state.version)?;
                state.schema_state = Some(request);
                write_schema_state_snapshot(cli, &state)?;
                write_frame(control_writer, &PluginFrame::Installed).await?;
            }
            HostFrame::InstallSchemaDelta(delta) => {
                if let Some(schema_state) = state.schema_state.as_mut() {
                    schema_state.schema_state.version =
                        schema_state.schema_state.version.max(delta.version);
                    for (namespace, entry) in delta.namespaces {
                        if schema_state
                            .schema_state
                            .namespace_versions
                            .get(&namespace)
                            .is_some_and(|installed| *installed >= entry.version)
                        {
                            continue;
                        }
                        schema_state
                            .schema_state
                            .namespaces
                            .insert(namespace.clone(), entry.metadata);
                        schema_state
                            .schema_state
                            .namespace_versions
                            .insert(namespace, entry.version);
                    }
                }
                write_schema_state_snapshot(cli, &state)?;
                write_frame(control_writer, &PluginFrame::Installed).await?;
            }
            HostFrame::RunSchema(request) => {
                if cli.scenario == "crash_once" && should_crash_once(cli.marker_path.as_ref())? {
                    std::process::exit(1);
                }
                if cli.scenario == "disconnect_once" && should_crash_once(cli.marker_path.as_ref())?
                {
                    return Ok(());
                }
                state.request_ids.push(request.request_id);
                state.compaction_ids.push(request.compaction_id.clone());
                if cli.scenario == "refresh_once" && !state.refresh_requested_once {
                    state.refresh_requested_once = true;
                    write_schema_state_snapshot(cli, &state)?;
                    write_frame(
                        control_writer,
                        &PluginFrame::SchemaStateRefreshRequired(RuntimeSchemaRefreshRequest {
                            request_id: request.request_id,
                            namespace: request.namespace.clone(),
                            required_version: request.required_schema_version,
                            installed_version: state
                                .schema_state
                                .as_ref()
                                .and_then(|schema| {
                                    schema
                                        .schema_state
                                        .namespace_versions
                                        .get(&request.namespace)
                                        .copied()
                                })
                                .unwrap_or(0),
                        }),
                    )
                    .await?;
                    continue;
                }
                state.run_count += 1;
                write_schema_state_snapshot(cli, &state)?;
                write_frame(
                    control_writer,
                    &PluginFrame::SchemaAck(RuntimeRequestAck {
                        request_id: request.request_id,
                    }),
                )
                .await?;
            }
            HostFrame::Shutdown => return Ok(()),
            other => {
                write_frame(
                    control_writer,
                    &PluginFrame::Error(format!(
                        "unexpected schema frame after handshake: {:?}",
                        other
                    )),
                )
                .await?;
                return Err(io::Error::other("unexpected schema frame"));
            }
        }
    }
}

#[derive(Default)]
struct SchemaHelperState {
    install_request: Option<RuntimeSchemaInstallRequest>,
    schema_state: Option<RuntimeSchemaStateInstallRequest>,
    refresh_requested_once: bool,
    run_count: usize,
    request_ids: Vec<u64>,
    compaction_ids: Vec<String>,
}

async fn run_source_loop(
    cli: &TestHelperCli,
    control_reader: &mut OwnedReadHalf,
    control_writer: &mut OwnedWriteHalf,
    data_writer: &mut OwnedWriteHalf,
) -> io::Result<()> {
    let Some(frame) = read_frame_or_eof::<_, HostFrame>(control_reader).await? else {
        return Ok(());
    };
    match frame {
        HostFrame::RunSource(_) => match source_scenario(cli) {
            SourceScenario::CompleteOnly => {
                write_frame(
                    control_writer,
                    &PluginFrame::SourceEvent(SourceEvent::Completed),
                )
                .await
            }
            SourceScenario::EmitPeopleSinkWrite => {
                let arrow_stream_bytes = encode_record_batch_stream(people_arrow_stream()).await?;
                write_frame(
                    data_writer,
                    &PluginDataFrame::SinkWrite(RuntimeSourceSinkWrite {
                        filename: "runtime-helper-output".to_string(),
                        compaction_id: "runtime-helper-output".to_string(),
                        idempotency_key: "runtime-helper-output".to_string(),
                        wal_refs: Vec::new(),
                        write_semantics: SinkWriteSemantics::AtLeastOnce,
                        schema_fingerprint: String::new(),
                        arrow_stream_bytes,
                        cdc_ctx: None,
                        source_contract: None,
                    }),
                )
                .await?;
                write_frame(
                    control_writer,
                    &PluginFrame::SourceEvent(SourceEvent::Completed),
                )
                .await
            }
            SourceScenario::EmitPeopleSinkWriteAndCheckpoint => {
                let arrow_stream_bytes = encode_record_batch_stream(people_arrow_stream()).await?;
                write_frame(
                    data_writer,
                    &PluginDataFrame::SinkWrite(RuntimeSourceSinkWrite {
                        filename: "runtime-helper-output".to_string(),
                        compaction_id: "runtime-helper-output".to_string(),
                        idempotency_key: "runtime-helper-output".to_string(),
                        wal_refs: Vec::new(),
                        write_semantics: SinkWriteSemantics::AtLeastOnce,
                        schema_fingerprint: String::new(),
                        arrow_stream_bytes,
                        cdc_ctx: None,
                        source_contract: None,
                    }),
                )
                .await?;
                write_frame(
                    data_writer,
                    &PluginDataFrame::CheckpointUpdate {
                        update: RuntimeCheckpointUpdate {
                            key: "runtime-helper-source-checkpoint".to_string(),
                            envelope: cdc::CheckpointEnvelope::from_payload(
                                cdc::CheckpointAuthority::AdvisoryHint,
                                cdc::CheckpointKind::AdvisoryProgress,
                                1,
                                &b"checkpoint".to_vec(),
                            )
                            .expect("test helper checkpoint payload should serialize"),
                        },
                    },
                )
                .await?;
                write_frame(
                    control_writer,
                    &PluginFrame::SourceEvent(SourceEvent::Completed),
                )
                .await
            }
        },
        HostFrame::Shutdown => Ok(()),
        other => {
            write_frame(
                control_writer,
                &PluginFrame::Error(format!(
                    "unexpected source frame after handshake: {:?}",
                    other
                )),
            )
            .await?;
            Err(io::Error::other("unexpected source frame"))
        }
    }
}

#[derive(Clone, Copy)]
enum SourceScenario {
    CompleteOnly,
    EmitPeopleSinkWrite,
    EmitPeopleSinkWriteAndCheckpoint,
}

fn source_scenario(cli: &TestHelperCli) -> SourceScenario {
    match cli.scenario.as_str() {
        "emit_people_sink_write_and_checkpoint" => SourceScenario::EmitPeopleSinkWriteAndCheckpoint,
        "emit_people_sink_write" => SourceScenario::EmitPeopleSinkWrite,
        _ => SourceScenario::CompleteOnly,
    }
}

struct SingleBatchStream {
    schema: Arc<Schema>,
    batch: Option<RecordBatch>,
}

impl Stream for SingleBatchStream {
    type Item = Result<RecordBatch, DataFusionError>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        Poll::Ready(this.batch.take().map(Ok))
    }
}

impl RecordBatchStream for SingleBatchStream {
    fn schema(&self) -> Arc<Schema> {
        self.schema.clone()
    }
}

fn people_arrow_stream() -> SendableRecordBatchStream {
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("city", DataType::Utf8, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(Int64Array::from(vec![1, 2])) as _,
            Arc::new(StringArray::from(vec!["Ada Lovelace", "Grace Hopper"])) as _,
            Arc::new(StringArray::from(vec!["London", "New York"])) as _,
        ],
    )
    .expect("helper people batch should be valid");
    Box::pin(SingleBatchStream {
        schema,
        batch: Some(batch),
    })
}

struct HelperSinkPayload {
    bytes: Vec<u8>,
    rows: u64,
    byte_count: u64,
}

async fn read_sink_payload(
    reader: &mut OwnedReadHalf,
    request_id: u64,
) -> io::Result<HelperSinkPayload> {
    let mut bytes = Vec::new();
    let mut rows = 0u64;
    let mut expected_index = 0u32;
    loop {
        match read_frame_or_eof::<_, HostDataFrame>(reader).await? {
            Some(HostDataFrame::SinkChunk(chunk))
                if chunk.request_id == request_id && chunk.chunk_index == expected_index =>
            {
                chunk.validate_bound().map_err(io::Error::other)?;
                rows = rows.saturating_add(chunk.rows);
                bytes.extend_from_slice(&chunk.arrow_stream_bytes);
                expected_index = expected_index.saturating_add(1);
            }
            Some(HostDataFrame::FinishSink(finish)) if finish.request_id == request_id => {
                if finish.chunks != expected_index
                    || finish.rows != rows
                    || finish.bytes != bytes.len() as u64
                {
                    return Err(io::Error::other(
                        "runtime helper received mismatched FinishSink totals",
                    ));
                }
                return Ok(HelperSinkPayload {
                    byte_count: bytes.len() as u64,
                    bytes,
                    rows,
                });
            }
            Some(other) => {
                return Err(io::Error::other(format!(
                    "unexpected sink payload frame: {:?}",
                    other
                )));
            }
            None => {
                return Err(io::Error::other(
                    "runtime host closed sink data channel before FinishSink",
                ));
            }
        }
    }
}

async fn write_sink_request_to_output(
    arrow_stream_bytes: Vec<u8>,
    context: &skipprd::runtime_plugins::protocol::RuntimeExecutionContext,
) -> io::Result<()> {
    let mut stream = decode_record_batch_stream(arrow_stream_bytes)?;
    let schema = stream.schema();
    let mut batches = Vec::new();
    while let Some(batch) = stream.next().await {
        batches.push(batch.map_err(|err| io::Error::other(err.to_string()))?);
    }
    if batches.is_empty() {
        return Ok(());
    }

    let output_dir = PathBuf::from(&context.data_dir).join("output");
    std::fs::create_dir_all(&output_dir)?;

    let output_path = output_dir.join("runtime-helper-output.parquet");
    let file = std::fs::File::create(output_path)?;
    let mut writer = ArrowWriter::try_new(file, schema, None)
        .map_err(|err| io::Error::other(err.to_string()))?;
    for batch in &batches {
        writer
            .write(batch)
            .map_err(|err| io::Error::other(err.to_string()))?;
    }
    writer
        .close()
        .map_err(|err| io::Error::other(err.to_string()))?;
    Ok(())
}

fn write_sink_state_snapshot(cli: &TestHelperCli, state: &SinkHelperState) -> io::Result<()> {
    if matches!(
        cli.scenario.as_str(),
        "crash_once"
            | "disconnect_once"
            | "disconnect_on_schema_install_once"
            | "record_schema_installs"
    ) {
        return Ok(());
    }
    let Some(marker_path) = cli.marker_path.as_ref() else {
        return Ok(());
    };
    if let Some(parent) = marker_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        marker_path,
        serde_json::to_vec_pretty(&json!({
            "install_request": state.install_request,
            "schema_state": state.schema_state.as_ref().map(|request| &request.schema_state),
            "refresh_requested_once": state.refresh_requested_once,
            "run_count": state.run_count,
            "request_ids": state.request_ids,
            "compaction_ids": state.compaction_ids,
            "payload_bytes": state.payload_bytes,
        }))
        .map_err(|err| io::Error::other(err.to_string()))?,
    )?;
    Ok(())
}

fn write_schema_state_snapshot(cli: &TestHelperCli, state: &SchemaHelperState) -> io::Result<()> {
    if matches!(
        cli.scenario.as_str(),
        "crash_once"
            | "disconnect_once"
            | "disconnect_on_schema_install_once"
            | "record_schema_installs"
    ) {
        return Ok(());
    }
    let Some(marker_path) = cli.marker_path.as_ref() else {
        return Ok(());
    };
    if let Some(parent) = marker_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        marker_path,
        serde_json::to_vec_pretty(&json!({
            "install_request": state.install_request,
            "schema_state": state.schema_state.as_ref().map(|request| &request.schema_state),
            "refresh_requested_once": state.refresh_requested_once,
            "run_count": state.run_count,
            "request_ids": state.request_ids,
            "compaction_ids": state.compaction_ids,
        }))
        .map_err(|err| io::Error::other(err.to_string()))?,
    )?;
    Ok(())
}

fn record_schema_state_install(cli: &TestHelperCli, schema_version: u64) -> io::Result<()> {
    if cli.scenario != "record_schema_installs" {
        return Ok(());
    }
    let Some(marker_path) = cli.marker_path.as_ref() else {
        return Ok(());
    };
    if let Some(parent) = marker_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(marker_path)?;
    writeln!(file, "{schema_version}")?;
    Ok(())
}

fn should_crash_once(marker_path: Option<&PathBuf>) -> io::Result<bool> {
    let Some(marker_path) = marker_path else {
        return Ok(false);
    };

    if marker_path.exists() {
        return Ok(false);
    }

    if let Some(parent) = marker_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(marker_path, b"crashed")?;
    Ok(true)
}
