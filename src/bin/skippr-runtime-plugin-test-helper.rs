use std::io;
use std::path::PathBuf;

use clap::Parser;

use skippr::plugins::cdc;
use skippr::runtime_plugins::framing::{read_host_frame, write_plugin_frame};
use skippr::runtime_plugins::protocol::{
    HandshakeResponse, HostFrame, PluginFrame, RuntimePluginKind, SourceEvent,
    RUNTIME_PROTOCOL_VERSION,
};

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
            return Err(io::Error::other("test helper did not receive a handshake"));
        }
    };

    if handshake.protocol_version != RUNTIME_PROTOCOL_VERSION {
        return Err(io::Error::other("runtime protocol version mismatch"));
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
    write_plugin_frame(&mut stdout, &PluginFrame::HandshakeAck(response)).await?;

    match kind {
        RuntimePluginKind::DataSink => run_sink_loop(&cli, &mut stdin, &mut stdout).await,
        RuntimePluginKind::SchemaSink => run_schema_loop(&mut stdin, &mut stdout).await,
        RuntimePluginKind::DataSource => run_source_loop(&mut stdin, &mut stdout).await,
    }
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

async fn run_sink_loop<R, W>(cli: &TestHelperCli, reader: &mut R, writer: &mut W) -> io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    loop {
        match read_host_frame(reader).await? {
            HostFrame::RunSink(_) => {
                if cli.scenario == "crash_once" && should_crash_once(cli.marker_path.as_ref())? {
                    std::process::exit(1);
                }
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

async fn run_schema_loop<R, W>(reader: &mut R, writer: &mut W) -> io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    loop {
        match read_host_frame(reader).await? {
            HostFrame::RunSchema(_) => write_plugin_frame(writer, &PluginFrame::SchemaAck).await?,
            HostFrame::Shutdown => return Ok(()),
            other => {
                write_plugin_frame(
                    writer,
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

async fn run_source_loop<R, W>(reader: &mut R, writer: &mut W) -> io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    match read_host_frame(reader).await? {
        HostFrame::RunSource(_) => {
            write_plugin_frame(writer, &PluginFrame::SourceEvent(SourceEvent::Completed)).await
        }
        HostFrame::Shutdown => Ok(()),
        other => {
            write_plugin_frame(
                writer,
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
