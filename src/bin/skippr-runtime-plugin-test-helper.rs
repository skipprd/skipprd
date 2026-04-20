use std::io;
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

use skippr::plugins::cdc;
use skippr::runtime_plugins::protocol::{
    HandshakeResponse, HostDataFrame, HostFrame, PluginDataFrame, PluginFrame,
    RuntimeCheckpointUpdate, RuntimePluginKind, RuntimeRequestAck, RuntimeSchemaInstallRequest,
    RuntimeSchemaRefreshRequest, RuntimeSchemaStateInstallRequest, RuntimeSessionHello,
    RuntimeSinkInstallRequest, RuntimeSourceSinkWrite, SourceEvent, RUNTIME_PROTOCOL_VERSION,
    SKIPPR_RUNTIME_CONTROL_ADDR_ENV, SKIPPR_RUNTIME_DATA_ADDR_ENV,
    SKIPPR_RUNTIME_SESSION_TOKEN_ENV,
};
use skippr::runtime_plugins::sdk::{decode_record_batch_stream, encode_record_batch_stream};
use skippr::runtime_plugins::wire::{read_frame_or_eof, write_frame};

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
            run_sink_loop(
                &cli,
                &mut control_reader,
                &mut control_writer,
                &mut data_reader,
            )
            .await
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
                state.schema_state = Some(request);
                write_sink_state_snapshot(cli, &state)?;
                write_frame(control_writer, &PluginFrame::Installed).await?;
            }
            HostFrame::RunSink(request) => {
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
                    write_sink_state_snapshot(cli, &state)?;
                    let _ = read_sink_payload(data_reader, request.request_id).await?;
                    write_frame(
                        control_writer,
                        &PluginFrame::SchemaStateRefreshRequired(RuntimeSchemaRefreshRequest {
                            required_version: request.required_schema_version,
                            installed_version: state
                                .schema_state
                                .as_ref()
                                .map(|schema| schema.schema_state.version)
                                .unwrap_or(0),
                        }),
                    )
                    .await?;
                    continue;
                }
                let arrow_stream_bytes = read_sink_payload(data_reader, request.request_id).await?;
                if matches!(sink_scenario(cli), SinkScenario::WriteOutputBufferParquet) {
                    let context = state
                        .install_request
                        .as_ref()
                        .map(|install| &install.context)
                        .ok_or_else(|| io::Error::other("sink run received before install"))?;
                    write_sink_request_to_output_buffer(arrow_stream_bytes, context).await?;
                }
                state.run_count += 1;
                write_sink_state_snapshot(cli, &state)?;
                write_frame(
                    control_writer,
                    &PluginFrame::SinkAck(RuntimeRequestAck {
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
}

#[derive(Clone, Copy)]
enum SinkScenario {
    AckOnly,
    WriteOutputBufferParquet,
}

fn sink_scenario(cli: &TestHelperCli) -> SinkScenario {
    match cli.scenario.as_str() {
        "write_output_buffer_parquet" => SinkScenario::WriteOutputBufferParquet,
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
                state.schema_state = Some(request);
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
                            required_version: request.required_schema_version,
                            installed_version: state
                                .schema_state
                                .as_ref()
                                .map(|schema| schema.schema_state.version)
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
                        arrow_stream_bytes,
                        cdc_ctx: None,
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
                        arrow_stream_bytes,
                        cdc_ctx: None,
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
        "emit_people_sink_write_and_checkpoint" => {
            SourceScenario::EmitPeopleSinkWriteAndCheckpoint
        }
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

async fn read_sink_payload(reader: &mut OwnedReadHalf, request_id: u64) -> io::Result<Vec<u8>> {
    match read_frame_or_eof::<_, HostDataFrame>(reader).await? {
        Some(HostDataFrame::SinkPayload(payload)) if payload.request_id == request_id => {
            Ok(payload.arrow_stream_bytes)
        }
        Some(HostDataFrame::SinkPayload(payload)) => Err(io::Error::other(format!(
            "sink payload request id mismatch: expected {} got {}",
            request_id, payload.request_id
        ))),
        None => Err(io::Error::other("runtime host closed sink data channel")),
    }
}

async fn write_sink_request_to_output_buffer(
    arrow_stream_bytes: Vec<u8>,
    context: &skippr::runtime_plugins::protocol::RuntimeExecutionContext,
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

    let output_dir = PathBuf::from(&context.data_dir).join("output_buffer");
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
        "crash_once" | "disconnect_once" | "disconnect_on_schema_install_once"
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

fn write_schema_state_snapshot(cli: &TestHelperCli, state: &SchemaHelperState) -> io::Result<()> {
    if matches!(
        cli.scenario.as_str(),
        "crash_once" | "disconnect_once" | "disconnect_on_schema_install_once"
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
