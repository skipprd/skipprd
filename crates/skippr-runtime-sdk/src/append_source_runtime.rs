use std::future::Future;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Parser;
use skippr_core::discover::OutputMetadata as CoreOutputMetadata;
use skippr_core::helpers::logging::init_logging;
use skippr_core::plugins::source_sync::SourceSyncContext;
use skippr_core::plugins::traits::SourceOnceContract;
use skippr_core::plugins::DataSource;
use skippr_core::{METADATA, PIPELINE_SCHEMA_VERSION};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::runtime::Handle;
use tokio::sync::{watch, Mutex};

use crate::protocol::{
    HandshakeResponse, HostFrame, PluginDataFrame, PluginFrame, RuntimeExecutionMode,
    RuntimePluginConfigEnvelope, RuntimePluginKind, RuntimeSchemaState, RuntimeSessionHello,
    RuntimeSourceCapabilityDescriptor, SourceEvent, SourceStartRequest, RUNTIME_PROTOCOL_VERSION,
    SKIPPR_RUNTIME_CONTROL_ADDR_ENV, SKIPPR_RUNTIME_DATA_ADDR_ENV,
    SKIPPR_RUNTIME_EXECUTION_MODE_ENV, SKIPPR_RUNTIME_SESSION_TOKEN_ENV,
};
use crate::source_sync::{
    run_offset_service_reader_loop, RuntimeIngestAckClient, RuntimeSourceSyncContext,
};
use crate::wire::{read_frame_or_eof, write_frame};

#[derive(Debug, Parser)]
struct AppendSourceCli {}

const DEFAULT_ONCE_IDLE_TIMEOUT_SECONDS: u64 = 60;
const ONCE_IDLE_TIMEOUT_SECONDS_ENV: &str = "SKIPPR_RUNTIME_ONCE_IDLE_TIMEOUT_SECONDS";

fn configure_runtime_source_data_dir(base_data_dir: &str, plugin_name: &str) {
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
    let runtime_data_dir = format!("{base_data_dir}/runtime_source_children/{plugin_slug}");
    let _ = std::fs::create_dir_all(&runtime_data_dir);
    std::env::set_var("DATA_DIR", runtime_data_dir);
}

fn configure_runtime_input_config(config: &RuntimePluginConfigEnvelope) {
    std::env::set_var("SKIPPR_RUNTIME_INPUT_PLUGIN_NAME", &config.plugin_name);
    std::env::set_var("SKIPPR_RUNTIME_INPUT_CONFIG_JSON", &config.raw_config_json);
}

fn runtime_mode_suppresses_data_relay(execution_mode: RuntimeExecutionMode) -> bool {
    matches!(execution_mode, RuntimeExecutionMode::Discover)
}

fn runtime_once_idle_timeout() -> Duration {
    let seconds = std::env::var(ONCE_IDLE_TIMEOUT_SECONDS_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_ONCE_IDLE_TIMEOUT_SECONDS);
    Duration::from_secs(seconds)
}

/// Idle supervision applies only to streaming sources; bounded `sync()` sources use `Finite`.
fn runtime_once_idle_timeout_for_contract(
    once: bool,
    suppress_data_relay: bool,
    once_contract: SourceOnceContract,
) -> Option<Duration> {
    if !once || suppress_data_relay || once_contract == SourceOnceContract::Finite {
        return None;
    }
    Some(runtime_once_idle_timeout())
}

#[derive(Clone, Debug)]
struct RuntimeSourceActivity {
    last_source_data_ms: Arc<AtomicU64>,
}

impl RuntimeSourceActivity {
    fn new() -> Self {
        Self {
            last_source_data_ms: Arc::new(AtomicU64::new(Self::now_millis())),
        }
    }

    fn now_millis() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    #[cfg(test)]
    fn mark_source_data(&self) {
        self.last_source_data_ms
            .store(Self::now_millis(), Ordering::Release);
    }

    fn source_data_idle_for(&self) -> Duration {
        Duration::from_millis(
            Self::now_millis().saturating_sub(self.last_source_data_ms.load(Ordering::Acquire)),
        )
    }
}

async fn wait_for_runtime_source_once_idle(
    activity: RuntimeSourceActivity,
    idle_timeout: Duration,
) -> io::Result<()> {
    loop {
        let idle_for = activity.source_data_idle_for();
        if idle_for >= idle_timeout {
            return Ok(());
        }
        tokio::time::sleep(idle_timeout.saturating_sub(idle_for)).await;
    }
}

fn current_runtime_schema_state_from_core() -> RuntimeSchemaState {
    let metadata = METADATA.load();
    let version = PIPELINE_SCHEMA_VERSION.load(Ordering::Acquire);
    let namespaces: std::collections::BTreeMap<_, _> = metadata
        .metadata
        .iter()
        .map(|(namespace, schema)| {
            let output = if metadata.flattened {
                CoreOutputMetadata::from_flatterened_metadata(schema)
            } else {
                CoreOutputMetadata::from_metadata(schema)
            };
            (namespace.clone(), output.into())
        })
        .collect();
    RuntimeSchemaState {
        version,
        namespace_versions: namespaces
            .keys()
            .map(|namespace| (namespace.clone(), version))
            .collect(),
        namespaces,
    }
}

pub(crate) fn block_on_handle<F, T>(handle: &Handle, future: F) -> T
where
    F: Future<Output = T>,
{
    if Handle::try_current().is_ok() {
        tokio::task::block_in_place(|| handle.block_on(future))
    } else {
        handle.block_on(future)
    }
}

#[derive(Clone)]
pub(crate) struct ControlWriter {
    pub(crate) handle: Handle,
    writer: Arc<Mutex<OwnedWriteHalf>>,
}

impl ControlWriter {
    fn new(writer: OwnedWriteHalf) -> Self {
        Self {
            handle: Handle::current(),
            writer: Arc::new(Mutex::new(writer)),
        }
    }

    pub(crate) async fn write(&self, frame: &PluginFrame) -> io::Result<()> {
        let mut guard = self.writer.lock().await;
        write_frame(&mut *guard, frame).await
    }
}

#[derive(Clone)]
pub(crate) struct DataWriter {
    writer: Arc<Mutex<OwnedWriteHalf>>,
}

impl DataWriter {
    fn new(writer: OwnedWriteHalf) -> Self {
        Self {
            writer: Arc::new(Mutex::new(writer)),
        }
    }

    pub(crate) async fn write(&self, frame: &PluginDataFrame) -> io::Result<()> {
        let mut guard = self.writer.lock().await;
        write_frame(&mut *guard, frame).await
    }
}

struct RuntimeSourceControl {
    shutdown_tx: watch::Sender<bool>,
    shutdown_error: StdMutex<Option<String>>,
}

impl RuntimeSourceControl {
    fn new() -> Arc<Self> {
        let (shutdown_tx, _) = watch::channel(false);
        Arc::new(Self {
            shutdown_tx,
            shutdown_error: StdMutex::new(None),
        })
    }

    fn subscribe_shutdown(&self) -> watch::Receiver<bool> {
        self.shutdown_tx.subscribe()
    }

    fn shutdown(&self, error: Option<String>) {
        if let Some(error) = error {
            *self.shutdown_error.lock().unwrap() = Some(error);
        }
        let _ = self.shutdown_tx.send(true);
    }

    fn shutdown_error(&self) -> Option<String> {
        self.shutdown_error.lock().unwrap().clone()
    }
}

async fn run_runtime_source_host_frame_loop(
    mut reader: OwnedReadHalf,
    control: Arc<RuntimeSourceControl>,
    ingest_ack_client: Arc<RuntimeIngestAckClient>,
) -> io::Result<()> {
    loop {
        match read_frame_or_eof::<_, HostFrame>(&mut reader).await? {
            Some(HostFrame::IngestAck(ack)) => {
                ingest_ack_client.resolve_ack(ack);
            }
            Some(HostFrame::Shutdown) | None => {
                ingest_ack_client.fail_all("runtime source host shut down".to_string());
                control.shutdown(None);
                return Ok(());
            }
            Some(other) => {
                let err = io::Error::other(format!(
                    "unexpected host control frame after source start: {:?}",
                    other
                ));
                ingest_ack_client.fail_all(err.to_string());
                control.shutdown(Some(err.to_string()));
                return Err(err);
            }
        }
    }
}

async fn wait_for_runtime_source_shutdown(
    shutdown_rx: &mut watch::Receiver<bool>,
    control: Arc<RuntimeSourceControl>,
) -> io::Result<()> {
    if !*shutdown_rx.borrow() {
        shutdown_rx
            .changed()
            .await
            .map_err(|_| io::Error::other("runtime source shutdown watcher dropped"))?;
    }
    if let Some(err) = control.shutdown_error() {
        Err(io::Error::other(err))
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn current_runtime_source_parent_pid() -> u32 {
    unsafe { libc::getppid() as u32 }
}

#[cfg(not(unix))]
fn current_runtime_source_parent_pid() -> u32 {
    0
}

#[cfg(unix)]
async fn wait_for_runtime_source_parent_exit(parent_pid: u32) -> io::Result<()> {
    loop {
        if current_runtime_source_parent_pid() != parent_pid {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

#[cfg(unix)]
async fn wait_for_runtime_source_host_exit(
    shutdown_rx: &mut watch::Receiver<bool>,
    control: Arc<RuntimeSourceControl>,
    parent_pid: u32,
) -> io::Result<()> {
    tokio::select! {
        result = wait_for_runtime_source_shutdown(shutdown_rx, control) => result,
        result = wait_for_runtime_source_parent_exit(parent_pid) => result,
    }
}

#[cfg(not(unix))]
async fn wait_for_runtime_source_host_exit(
    shutdown_rx: &mut watch::Receiver<bool>,
    control: Arc<RuntimeSourceControl>,
    _parent_pid: u32,
) -> io::Result<()> {
    wait_for_runtime_source_shutdown(shutdown_rx, control).await
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
    let parent_pid = current_runtime_source_parent_pid();

    let control_stream = connect_runtime_channel(SKIPPR_RUNTIME_CONTROL_ADDR_ENV).await?;
    let data_stream = connect_runtime_channel(SKIPPR_RUNTIME_DATA_ADDR_ENV).await?;
    let (mut control_reader, control_writer_raw) = control_stream.into_split();
    let (_data_reader, data_writer_raw) = data_stream.into_split();

    let Some(handshake_frame) = read_frame_or_eof::<_, HostFrame>(&mut control_reader).await?
    else {
        return Ok(());
    };
    let handshake = match handshake_frame {
        HostFrame::Handshake(handshake) => handshake,
        other => {
            let writer = ControlWriter::new(control_writer_raw);
            writer
                .write(&PluginFrame::Error(format!(
                    "expected handshake as first frame, got {:?}",
                    other
                )))
                .await?;
            return Err(io::Error::other(format!(
                "{} did not receive a handshake",
                binary_label
            )));
        }
    };

    if handshake.protocol_version != RUNTIME_PROTOCOL_VERSION {
        let writer = ControlWriter::new(control_writer_raw);
        writer
            .write(&PluginFrame::Error(format!(
                "protocol version mismatch: host={} child={}",
                handshake.protocol_version, RUNTIME_PROTOCOL_VERSION
            )))
            .await?;
        return Err(io::Error::other("runtime protocol version mismatch"));
    }

    let control_writer = ControlWriter::new(control_writer_raw);
    let data_writer = DataWriter::new(data_writer_raw);

    control_writer
        .write(&PluginFrame::HandshakeAck(HandshakeResponse {
            protocol_version: RUNTIME_PROTOCOL_VERSION,
            kind: RuntimePluginKind::DataSource,
            plugin_name: plugin_name.to_string(),
            source_capability,
            sink_capability: None,
            supports_schema: false,
        }))
        .await?;

    let Some(start_frame) = read_frame_or_eof::<_, HostFrame>(&mut control_reader).await? else {
        return Ok(());
    };
    let start = match start_frame {
        HostFrame::RunSource(start) => start,
        HostFrame::Shutdown => return Ok(()),
        other => {
            control_writer
                .write(&PluginFrame::Error(format!(
                    "expected RunSource after handshake, got {:?}",
                    other
                )))
                .await?;
            return Err(io::Error::other("unexpected source frame"));
        }
    };

    configure_runtime_source_data_dir(&start.context.data_dir, plugin_name);
    configure_runtime_input_config(&start.config.0);
    let execution_mode_label = match start.context.execution_mode {
        RuntimeExecutionMode::Discover => "discover",
        RuntimeExecutionMode::Sync => "sync",
    };
    std::env::set_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV, execution_mode_label);
    let suppress_data_relay = runtime_mode_suppresses_data_relay(start.context.execution_mode);

    let control = RuntimeSourceControl::new();
    let mut shutdown_rx = control.subscribe_shutdown();
    let reader_control = control.clone();
    let ingest_ack_client = Arc::new(RuntimeIngestAckClient::new(
        start.source_ingest_window.clone(),
    ));
    let reader_ingest_ack_client = ingest_ack_client.clone();
    tokio::spawn(async move {
        let _ = run_runtime_source_host_frame_loop(
            control_reader,
            reader_control,
            reader_ingest_ack_client,
        )
        .await;
    });

    let source_once = start.once;
    let mut source = build(start).await?;
    let once_idle_timeout = runtime_once_idle_timeout_for_contract(
        source_once,
        suppress_data_relay,
        source.execution_contract().once,
    );
    let namespace_contracts = source.source_namespace_contracts();
    if !namespace_contracts.is_empty() {
        control_writer
            .write(&PluginFrame::SourceEvent(SourceEvent::ContractsUpdate(
                namespace_contracts,
            )))
            .await?;
    }
    let (sync_ctx, offset_reader) = RuntimeSourceSyncContext::new(
        control_writer.clone(),
        data_writer.clone(),
        ingest_ack_client,
        suppress_data_relay,
    )
    .await?;
    let offset_client = sync_ctx.offset_client();
    tokio::spawn(async move {
        let _ = run_offset_service_reader_loop(offset_reader, offset_client).await;
    });
    let activity = RuntimeSourceActivity::new();

    let sync_ctx: Arc<dyn SourceSyncContext> = Arc::new(sync_ctx);
    let sync_result = {
        let sync_fut = source.sync(sync_ctx.clone());
        tokio::pin!(sync_fut);
        tokio::select! {
            result = &mut sync_fut => Some(result),
            result = wait_for_runtime_source_host_exit(&mut shutdown_rx, control.clone(), parent_pid) => match result {
                Ok(()) => None,
                Err(err) => Some(Err(err)),
            },
            result = async {
                match once_idle_timeout {
                    Some(idle_timeout) => {
                        wait_for_runtime_source_once_idle(activity.clone(), idle_timeout).await
                    }
                    None => std::future::pending::<io::Result<()>>().await,
                }
            } => match result {
                Ok(()) => Some(Ok(())),
                Err(err) => Some(Err(err)),
            },
        }
    };

    drop(source);

    let Some(sync_result) = sync_result else {
        return Ok(());
    };

    match sync_result {
        Ok(()) => {
            sync_ctx.drain_payload_acks()?;
            if suppress_data_relay {
                control_writer
                    .write(&PluginFrame::SourceEvent(SourceEvent::SchemaStateUpdate(
                        current_runtime_schema_state_from_core(),
                    )))
                    .await?;
            }
            control_writer
                .write(&PluginFrame::SourceEvent(SourceEvent::Completed))
                .await?;
        }
        Err(err) => {
            control_writer
                .write(&PluginFrame::Error(err.to_string()))
                .await?;
            return Err(err);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::protocol::{
        RuntimeExecutionContext, RuntimeOutputLayout, RuntimeSourceConfig,
        RuntimeSourceIngestWindow,
    };

    fn source_start_request(once: bool) -> SourceStartRequest {
        SourceStartRequest {
            context: RuntimeExecutionContext {
                pipeline_name: "pipeline".to_string(),
                workspace_name: "workspace".to_string(),
                data_dir: "/tmp/skippr-runtime-test".to_string(),
                execution_mode: RuntimeExecutionMode::Sync,
                output_layout: RuntimeOutputLayout::default(),
                inject_fields: Default::default(),
            },
            config: RuntimeSourceConfig(RuntimePluginConfigEnvelope::new("Test", json!({}))),
            once,
            source_ingest_window: RuntimeSourceIngestWindow::default(),
        }
    }

    #[test]
    fn runtime_source_start_preserves_once_flag() {
        assert!(source_start_request(true).once);
        assert!(!source_start_request(false).once);
    }

    #[test]
    fn finite_once_sources_skip_idle_supervision() {
        assert!(
            runtime_once_idle_timeout_for_contract(true, false, SourceOnceContract::Finite,)
                .is_none()
        );
        assert!(runtime_once_idle_timeout_for_contract(
            true,
            false,
            SourceOnceContract::HostIdleBounded,
        )
        .is_some());
        assert!(runtime_once_idle_timeout_for_contract(
            true,
            true,
            SourceOnceContract::HostIdleBounded,
        )
        .is_none());
    }

    #[test]
    fn runtime_source_activity_resets_idle_clock_on_source_data() {
        let activity = RuntimeSourceActivity::new();
        activity.last_source_data_ms.store(
            RuntimeSourceActivity::now_millis().saturating_sub(1_000),
            Ordering::Release,
        );

        assert!(activity.source_data_idle_for() >= Duration::from_millis(1_000));

        activity.mark_source_data();

        assert!(activity.source_data_idle_for() < Duration::from_millis(1_000));
    }
}
