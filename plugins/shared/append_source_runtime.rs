//! Shared stdin/stdout loop for append-style data sources that drive the core
//! ingest path and forward Arrow IPC to the host via `SourceEvent::SinkWrite`.

use std::io;
use std::sync::Arc;

use async_trait::async_trait;
use clap::Parser;
use datafusion::execution::SendableRecordBatchStream;
use skippr::buffer::ingest_buffer::Buffers;
use skippr::helpers::configuration::{Config, PIPELINE_NAME};
use skippr::helpers::logging::init_logging;
use skippr::helpers::offsets::Offsets;
use skippr::plugins::cdc::SyncContext;
use skippr::plugins::{DataSink, DataSource};
use skippr::runtime_plugins::framing::{read_host_frame, write_plugin_frame};
use skippr::runtime_plugins::protocol::RuntimePluginKind;
use skippr::runtime_plugins::protocol::{
    HandshakeResponse, HostFrame, PluginFrame, RuntimePluginConfigEnvelope,
    RuntimeSourceCapabilityDescriptor, SourceEvent, SourceStartRequest, RUNTIME_PROTOCOL_VERSION,
};
use skippr::runtime_plugins::sdk::encode_record_batch_stream;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;

#[derive(Debug, Parser)]
struct AppendSourceCli {}

fn configure_runtime_source_data_dir(plugin_name: &str) {
    let base_data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "./data".to_string());
    let plugin_slug = plugin_name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    std::env::set_var(
        "DATA_DIR",
        format!("{base_data_dir}/runtime_source_children/{plugin_slug}"),
    );
}

fn configure_runtime_input_config(config: &RuntimePluginConfigEnvelope) {
    std::env::set_var("SKIPPR_RUNTIME_INPUT_PLUGIN_NAME", &config.plugin_name);
    std::env::set_var("SKIPPR_RUNTIME_INPUT_CONFIG_JSON", &config.raw_config_json);
}

/// Forwards each record batch stream to the host as Arrow IPC.
pub struct ArrowRelayToHostSink<W> {
    writer: Arc<Mutex<W>>,
}

impl<W> ArrowRelayToHostSink<W> {
    pub fn new(writer: Arc<Mutex<W>>) -> Self {
        Self { writer }
    }
}

#[async_trait]
impl<W> DataSink for ArrowRelayToHostSink<W>
where
    W: AsyncWrite + Unpin + Send + Sync + 'static,
{
    async fn sync(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
        cdc_ctx: Option<&SyncContext>,
    ) -> Result<(), io::Error> {
        let arrow_stream_bytes = encode_record_batch_stream(stream).await?;
        if std::env::var("SKIPPR_RUNTIME_LOG_LEVEL")
            .map(|level| level.eq_ignore_ascii_case("debug"))
            .unwrap_or(false)
        {
            eprintln!(
                "{}: relaying sink write filename={} bytes={}",
                env!("CARGO_PKG_NAME"),
                filename,
                arrow_stream_bytes.len()
            );
        }
        let mut guard = self.writer.lock().await;
        write_plugin_frame(
            &mut *guard,
            &PluginFrame::SourceEvent(SourceEvent::SinkWrite {
                filename,
                arrow_stream_bytes,
                cdc_ctx: cdc_ctx.cloned(),
            }),
        )
        .await?;
        Ok(())
    }
}

/// Handshake + `RunSource` for append-style sources using [`DataSource::sync`].
pub async fn run_append_data_source_main(
    binary_label: &'static str,
    plugin_name: &'static str,
    source_capability: Option<RuntimeSourceCapabilityDescriptor>,
    build: impl FnOnce(
        SourceStartRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Box<dyn DataSource + Send>, io::Error>> + Send>,
    >,
) -> io::Result<()> {
    let _cli = AppendSourceCli::parse();
    let log_level =
        std::env::var("SKIPPR_RUNTIME_LOG_LEVEL").unwrap_or_else(|_| "warn".to_string());
    init_logging(Some(log_level));

    let pipeline_name = std::env::var("PIPELINE_NAME").unwrap_or_else(|_| "default".to_string());
    {
        let mut current = PIPELINE_NAME.write();
        current.clear();
        current.push_str(&pipeline_name);
    }

    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();

    let handshake = match read_host_frame(&mut stdin).await? {
        HostFrame::Handshake(handshake) => handshake,
        other => {
            write_plugin_frame(
                &mut stdout,
                &PluginFrame::Error(format!(
                    "expected handshake as first frame, got {:?}",
                    other
                )),
            )
            .await?;
            return Err(io::Error::other(format!(
                "{} did not receive a handshake",
                binary_label
            )));
        }
    };

    if handshake.protocol_version != RUNTIME_PROTOCOL_VERSION {
        write_plugin_frame(
            &mut stdout,
            &PluginFrame::Error(format!(
                "protocol version mismatch: host={} child={}",
                handshake.protocol_version, RUNTIME_PROTOCOL_VERSION
            )),
        )
        .await?;
        return Err(io::Error::other("runtime protocol version mismatch"));
    }

    let response = HandshakeResponse {
        protocol_version: RUNTIME_PROTOCOL_VERSION,
        kind: RuntimePluginKind::DataSource,
        plugin_name: plugin_name.to_string(),
        source_capability,
        sink_capability: None,
        supports_schema: false,
    };
    write_plugin_frame(&mut stdout, &PluginFrame::HandshakeAck(response)).await?;

    let start = match read_host_frame(&mut stdin).await? {
        HostFrame::RunSource(start) => start,
        HostFrame::Shutdown => return Ok(()),
        other => {
            write_plugin_frame(
                &mut stdout,
                &PluginFrame::Error(format!(
                    "expected RunSource after handshake, got {:?}",
                    other
                )),
            )
            .await?;
            return Err(io::Error::other("unexpected source frame"));
        }
    };

    // Runtime source children must not contend with the host's offsets DB lock.
    configure_runtime_source_data_dir(plugin_name);
    configure_runtime_input_config(&start.config.0);
    Config::reset_envcache();
    Config::build_config();
    Config::init().await;
    let stdout = Arc::new(Mutex::new(stdout));
    let mut source = build(start).await?;
    let offsets = Arc::new(Offsets::init().map_err(io::Error::other)?);
    let relay: Arc<Box<dyn DataSink + Send + Sync>> =
        Arc::new(Box::new(ArrowRelayToHostSink::new(stdout.clone())));
    Buffers::start_compactor_service(relay.clone(), offsets.clone());
    let sync_result = source.sync(offsets.clone(), relay.clone()).await;
    drop(source);
    let drain_result = Buffers::drain_and_stop_compactor(offsets.clone()).await;

    let mut out = stdout.lock().await;
    match sync_result {
        Ok(()) => {
            if !drain_result {
                write_plugin_frame(
                    &mut *out,
                    &PluginFrame::Error("runtime source failed to drain WAL compactor".to_string()),
                )
                .await?;
                return Err(io::Error::other("runtime source failed to drain WAL compactor"));
            }
            write_plugin_frame(&mut *out, &PluginFrame::SourceEvent(SourceEvent::Completed))
                .await?;
        }
        Err(err) => {
            write_plugin_frame(&mut *out, &PluginFrame::Error(err.to_string())).await?;
            return Err(err);
        }
    }
    let _ = out.flush().await;
    Ok(())
}
