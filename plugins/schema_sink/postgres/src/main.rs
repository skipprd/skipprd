use std::io;

use clap::Parser;
use skippr::helpers::configuration::PIPELINE_NAME;
use skippr::helpers::logging::init_logging;
use skippr::runtime_plugins::framing::{read_host_frame, write_plugin_frame};
use skippr::runtime_plugins::protocol::{
    HandshakeResponse, HostFrame, PluginFrame, RuntimeBinding, RuntimePluginKind, SchemaRunRequest,
    RUNTIME_PROTOCOL_VERSION,
};
use skippr_plugin_data_sink_postgres::{DataSinkPostgresPlugin, DataSinkPostgresPluginConfig};

#[derive(Debug, Parser)]
struct PostgresSchemaRuntimePluginCli {}

fn buffer_name_for_binding(binding: RuntimeBinding) -> String {
    match binding {
        RuntimeBinding::Primary => "output".to_string(),
        RuntimeBinding::Deadletter => "deadletters".to_string(),
    }
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("skippr-plugin-schema-sink-postgres: {}", err);
        std::process::exit(1);
    }
}

async fn run() -> io::Result<()> {
    let _cli = PostgresSchemaRuntimePluginCli::parse();
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
                "runtime postgres schema sink did not receive a handshake",
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
        kind: RuntimePluginKind::SchemaSink,
        plugin_name: "Postgres".to_string(),
        source_capability: None,
        sink_capability: None,
        supports_schema: true,
    };
    write_plugin_frame(&mut stdout, &PluginFrame::HandshakeAck(response)).await?;

    run_schema_loop(&mut stdin, &mut stdout).await
}

async fn run_schema_loop<R, W>(reader: &mut R, writer: &mut W) -> io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut primary_plugin: Option<DataSinkPostgresPlugin> = None;
    let mut deadletter_plugin: Option<DataSinkPostgresPlugin> = None;

    loop {
        match read_host_frame(reader).await? {
            HostFrame::RunSchema(request) => {
                run_schema_request(request, &mut primary_plugin, &mut deadletter_plugin).await?;
                write_plugin_frame(writer, &PluginFrame::SchemaAck).await?;
            }
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

async fn run_schema_request(
    request: SchemaRunRequest,
    primary_plugin: &mut Option<DataSinkPostgresPlugin>,
    deadletter_plugin: &mut Option<DataSinkPostgresPlugin>,
) -> io::Result<()> {
    request
        .config
        .0
        .expect_plugin("Postgres")
        .map_err(io::Error::other)?;

    let plugin_slot = match request.binding {
        RuntimeBinding::Primary => primary_plugin,
        RuntimeBinding::Deadletter => deadletter_plugin,
    };
    if plugin_slot.is_none() {
        let config: DataSinkPostgresPluginConfig =
            request.config.0.decode().map_err(io::Error::other)?;
        *plugin_slot = Some(
            DataSinkPostgresPlugin::new_with_config(
                buffer_name_for_binding(request.binding),
                config,
            )
            .await,
        );
    }

    plugin_slot
        .as_ref()
        .unwrap()
        .sync_schema(&request.namespace, &request.metadata)
        .await
}
