//! Shared stdin/stdout runtime loops for external data sink and schema sink binaries.

use std::future::Future;
use std::io;

use serde_derive::Deserialize;

use crate::helpers::configuration::PIPELINE_NAME;
use crate::helpers::logging::init_logging;
use crate::plugins::cdc;
use crate::plugins::{DataSink, SchemaSink};
use crate::runtime_plugins::framing::{read_host_frame, write_plugin_frame};
use crate::runtime_plugins::protocol::{
    HandshakeResponse, HostFrame, PluginFrame, RuntimeBinding, RuntimePluginConfigEnvelope,
    RuntimePluginKind, SchemaRunRequest, SinkRunRequest, RUNTIME_PROTOCOL_VERSION,
};
use crate::runtime_plugins::sdk::decode_record_batch_stream;

/// Empty config for sinks that do not read JSON fields at runtime (e.g. Stdout).
#[derive(Debug, Deserialize)]
pub struct EmptyStdoutConfig {}

fn buffer_name_for_binding(binding: RuntimeBinding) -> String {
    match binding {
        RuntimeBinding::Primary => "output".to_string(),
        RuntimeBinding::Deadletter => "deadletters".to_string(),
    }
}

pub async fn run_stdio_data_sink_plugin<P, F, Fut>(
    expect_plugin_name: &'static str,
    handshake_display_name: &'static str,
    supports_schema: bool,
    bin_name: &'static str,
    mut build: F,
) -> io::Result<()>
where
    P: DataSink + Send + Sync,
    F: FnMut(RuntimeBinding, String, RuntimePluginConfigEnvelope) -> Fut,
    Fut: Future<Output = io::Result<P>>,
{
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
                "{}: runtime sink did not receive a handshake",
                bin_name
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
        kind: RuntimePluginKind::DataSink,
        plugin_name: handshake_display_name.to_string(),
        source_capability: None,
        sink_capability: cdc::sink_capabilities::by_name(handshake_display_name).map(Into::into),
        supports_schema,
    };
    write_plugin_frame(&mut stdout, &PluginFrame::HandshakeAck(response)).await?;

    let mut primary_plugin: Option<P> = None;
    let mut deadletter_plugin: Option<P> = None;

    loop {
        match read_host_frame(&mut stdin).await? {
            HostFrame::RunSink(request) => {
                run_sink_request(
                    expect_plugin_name,
                    request,
                    &mut primary_plugin,
                    &mut deadletter_plugin,
                    &mut build,
                )
                .await?;
                write_plugin_frame(&mut stdout, &PluginFrame::SinkAck).await?;
            }
            HostFrame::Shutdown => return Ok(()),
            other => {
                write_plugin_frame(
                    &mut stdout,
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

async fn run_sink_request<P, F, Fut>(
    expect_plugin_name: &'static str,
    request: SinkRunRequest,
    primary_plugin: &mut Option<P>,
    deadletter_plugin: &mut Option<P>,
    build: &mut F,
) -> io::Result<()>
where
    P: DataSink + Send + Sync,
    F: FnMut(RuntimeBinding, String, RuntimePluginConfigEnvelope) -> Fut,
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
    if plugin_slot.is_none() {
        *plugin_slot = Some(
            build(
                request.binding,
                buffer_name_for_binding(request.binding),
                request.config.0.clone(),
            )
            .await?,
        );
    }

    let stream = decode_record_batch_stream(request.arrow_stream_bytes)?;

    plugin_slot
        .as_ref()
        .unwrap()
        .sync(stream, request.filename, request.cdc_ctx.as_ref())
        .await
}

pub async fn run_stdio_schema_sink_plugin<P, F, Fut>(
    expect_plugin_name: &'static str,
    handshake_display_name: &'static str,
    bin_name: &'static str,
    mut build: F,
) -> io::Result<()>
where
    P: SchemaSink + Send + Sync,
    F: FnMut(RuntimeBinding, String, RuntimePluginConfigEnvelope) -> Fut,
    Fut: Future<Output = io::Result<P>>,
{
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
                "{}: runtime schema sink did not receive a handshake",
                bin_name
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
        kind: RuntimePluginKind::SchemaSink,
        plugin_name: handshake_display_name.to_string(),
        source_capability: None,
        sink_capability: None,
        supports_schema: true,
    };
    write_plugin_frame(&mut stdout, &PluginFrame::HandshakeAck(response)).await?;

    let mut primary_plugin: Option<P> = None;
    let mut deadletter_plugin: Option<P> = None;

    loop {
        match read_host_frame(&mut stdin).await? {
            HostFrame::RunSchema(request) => {
                run_schema_request(
                    expect_plugin_name,
                    request,
                    &mut primary_plugin,
                    &mut deadletter_plugin,
                    &mut build,
                )
                .await?;
                write_plugin_frame(&mut stdout, &PluginFrame::SchemaAck).await?;
            }
            HostFrame::Shutdown => return Ok(()),
            other => {
                write_plugin_frame(
                    &mut stdout,
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

async fn run_schema_request<P, F, Fut>(
    expect_plugin_name: &'static str,
    request: SchemaRunRequest,
    primary_plugin: &mut Option<P>,
    deadletter_plugin: &mut Option<P>,
    build: &mut F,
) -> io::Result<()>
where
    P: SchemaSink + Send + Sync,
    F: FnMut(RuntimeBinding, String, RuntimePluginConfigEnvelope) -> Fut,
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
    if plugin_slot.is_none() {
        *plugin_slot = Some(
            build(
                request.binding,
                buffer_name_for_binding(request.binding),
                request.config.0.clone(),
            )
            .await?,
        );
    }

    plugin_slot
        .as_ref()
        .unwrap()
        .sync_schema(&request.namespace, &request.metadata)
        .await
}
