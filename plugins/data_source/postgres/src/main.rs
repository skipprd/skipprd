mod pgoutput;
mod source;

use std::io;

use clap::Parser;
use skippr::helpers::configuration::PIPELINE_NAME;
use skippr::helpers::logging::init_logging;
use skippr::plugins::cdc;
use skippr::runtime_plugins::framing::{read_host_frame, write_plugin_frame};
use skippr::runtime_plugins::protocol::{
    HandshakeResponse, HostFrame, PluginFrame, RuntimePluginKind, RUNTIME_PROTOCOL_VERSION,
};

#[derive(Debug, Parser)]
struct PostgresSourceRuntimePluginCli {}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("skippr-plugin-data-source-postgres: {}", err);
        std::process::exit(1);
    }
}

async fn run() -> io::Result<()> {
    let _cli = PostgresSourceRuntimePluginCli::parse();
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
                "runtime postgres source did not receive a handshake",
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
        kind: RuntimePluginKind::DataSource,
        plugin_name: "Postgres".to_string(),
        source_capability: cdc::source_capabilities::by_name("Postgres").map(Into::into),
        sink_capability: None,
        supports_schema: false,
    };
    write_plugin_frame(&mut stdout, &PluginFrame::HandshakeAck(response)).await?;

    run_source_loop(&mut stdin, &mut stdout).await
}

async fn run_source_loop<R, W>(reader: &mut R, writer: &mut W) -> io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    match read_host_frame(reader).await? {
        HostFrame::RunSource(start) => {
            start
                .config
                .0
                .expect_plugin("Postgres")
                .map_err(io::Error::other)?;
            let config: source::DataSourcePostgresPluginConfig =
                start.config.0.decode().map_err(io::Error::other)?;
            source::run_runtime_postgres_source(writer, config, start).await
        }
        HostFrame::Shutdown => Ok(()),
        other => {
            write_plugin_frame(
                writer,
                &PluginFrame::Error(format!(
                    "expected RunSource after handshake, got {:?}",
                    other
                )),
            )
            .await?;
            Err(io::Error::other("unexpected source frame"))
        }
    }
}
