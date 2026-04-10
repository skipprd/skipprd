use std::io;

use clap::Parser;
use skippr::helpers::configuration::PIPELINE_NAME;
use skippr::helpers::logging::init_logging;
use skippr::plugins::cdc;
use skippr::plugins::DataSink;
use skippr::runtime_plugins::framing::{read_host_frame, write_plugin_frame};
use skippr::runtime_plugins::protocol::{
    HandshakeResponse, HostFrame, PluginFrame, RuntimeBinding, RuntimePluginKind, SinkRunRequest,
    RUNTIME_PROTOCOL_VERSION,
};
use skippr::runtime_plugins::sdk::decode_record_batch_stream;
use skippr_plugin_runtime_link::runtime_sink_link::stdout::DataSinkStdoutPlugin;

#[derive(Debug, Parser)]
struct StdoutSinkRuntimePluginCli {}

fn buffer_name_for_binding(binding: RuntimeBinding) -> String {
    match binding {
        RuntimeBinding::Primary => "output".to_string(),
        RuntimeBinding::Deadletter => "deadletters".to_string(),
    }
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("skippr-plugin-data-sink-stdout: {}", err);
        std::process::exit(1);
    }
}

async fn run() -> io::Result<()> {
    let _cli = StdoutSinkRuntimePluginCli::parse();
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
            return Err(io::Error::other(
                "runtime stdout sink did not receive a handshake",
            ));
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
        kind: RuntimePluginKind::DataSink,
        plugin_name: "Stdout".to_string(),
        source_capability: None,
        sink_capability: cdc::sink_capabilities::by_name("Stdout").map(Into::into),
        supports_schema: false,
    };
    write_plugin_frame(&mut stdout, &PluginFrame::HandshakeAck(response)).await?;

    run_sink_loop(&mut stdin, &mut stdout).await
}

async fn run_sink_loop<R, W>(reader: &mut R, writer: &mut W) -> io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut primary_plugin: Option<DataSinkStdoutPlugin> = None;
    let mut deadletter_plugin: Option<DataSinkStdoutPlugin> = None;

    loop {
        match read_host_frame(reader).await? {
            HostFrame::RunSink(request) => {
                run_sink_request(request, &mut primary_plugin, &mut deadletter_plugin).await?;
                write_plugin_frame(writer, &PluginFrame::SinkAck).await?;
            }
            HostFrame::Shutdown => return Ok(()),
            other => {
                write_plugin_frame(
                    writer,
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

async fn run_sink_request(
    request: SinkRunRequest,
    primary_plugin: &mut Option<DataSinkStdoutPlugin>,
    deadletter_plugin: &mut Option<DataSinkStdoutPlugin>,
) -> io::Result<()> {
    request
        .config
        .0
        .expect_plugin("Stdout")
        .map_err(io::Error::other)?;

    let plugin_slot = match request.binding {
        RuntimeBinding::Primary => primary_plugin,
        RuntimeBinding::Deadletter => deadletter_plugin,
    };
    if plugin_slot.is_none() {
        *plugin_slot =
            Some(DataSinkStdoutPlugin::new(buffer_name_for_binding(request.binding)).await);
    }

    let stream = decode_record_batch_stream(request.arrow_stream_bytes)?;

    plugin_slot
        .as_ref()
        .unwrap()
        .sync(stream, request.filename, request.cdc_ctx.as_ref())
        .await
}
