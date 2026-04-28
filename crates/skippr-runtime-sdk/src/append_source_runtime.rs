use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use async_trait::async_trait;
use clap::Parser;
use datafusion::execution::SendableRecordBatchStream;
use skippr_core::discover::OutputMetadata as CoreOutputMetadata;
use skippr_core::helpers::configuration::{Config, PIPELINE_NAME};
use skippr_core::helpers::logging::init_logging;
use skippr_core::helpers::offsets::{
    CheckpointTransport, OffsetTransport, Offsets, RuntimeOffsetOperation, RuntimeOffsetRpcRequest,
    RuntimeOffsetRpcResponse, RuntimeOffsetValue,
};
use skippr_core::plugins::cdc::{CheckpointEnvelope, SyncContext};
use skippr_core::plugins::{DataSink, DataSource, RuntimeIngestRelay};
use skippr_core::{METADATA, PIPELINE_SCHEMA_VERSION};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::runtime::Handle;
use tokio::sync::{watch, Mutex};

use crate::protocol::{
    HandshakeResponse, HostFrame, PluginDataFrame, PluginFrame, RuntimeCheckpointUpdate,
    RuntimeIngestPartitionBatch, RuntimeOffsetMaterializationHint, RuntimePluginConfigEnvelope,
    RuntimePluginKind, RuntimeSchemaState, RuntimeSessionHello, RuntimeSourceCapabilityDescriptor,
    RuntimeSourceSinkWrite, SourceEvent, SourceStartRequest, RUNTIME_PROTOCOL_VERSION,
    SKIPPR_RUNTIME_CONTROL_ADDR_ENV, SKIPPR_RUNTIME_DATA_ADDR_ENV,
    SKIPPR_RUNTIME_SESSION_TOKEN_ENV,
};
use crate::sdk::encode_record_batch_stream;
use crate::wire::{read_frame_or_eof, write_frame};

#[derive(Debug, Parser)]
struct AppendSourceCli {}

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

fn current_runtime_schema_state_from_core() -> RuntimeSchemaState {
    let metadata = METADATA.load();
    let namespaces = metadata
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
        version: PIPELINE_SCHEMA_VERSION.load(Ordering::Acquire),
        namespaces,
    }
}

fn compaction_id_for_filename(filename: &str) -> String {
    filename
        .rsplit_once("-c=")
        .map(|(_, suffix)| suffix.to_string())
        .unwrap_or_else(|| filename.to_string())
}

fn block_on_handle<F, T>(handle: &Handle, future: F) -> T
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
struct ControlWriter {
    handle: Handle,
    writer: Arc<Mutex<OwnedWriteHalf>>,
}

impl ControlWriter {
    fn new(writer: OwnedWriteHalf) -> Self {
        Self {
            handle: Handle::current(),
            writer: Arc::new(Mutex::new(writer)),
        }
    }

    async fn write(&self, frame: &PluginFrame) -> io::Result<()> {
        let mut guard = self.writer.lock().await;
        write_frame(&mut *guard, frame).await
    }
}

#[derive(Clone)]
struct DataWriter {
    writer: Arc<Mutex<OwnedWriteHalf>>,
}

impl DataWriter {
    fn new(writer: OwnedWriteHalf) -> Self {
        Self {
            writer: Arc::new(Mutex::new(writer)),
        }
    }

    async fn write(&self, frame: &PluginDataFrame) -> io::Result<()> {
        let mut guard = self.writer.lock().await;
        write_frame(&mut *guard, frame).await
    }
}

struct RuntimeSourceControl {
    pending: StdMutex<HashMap<u64, SyncSender<Result<RuntimeOffsetValue, String>>>>,
    next_request_id: AtomicU64,
    shutdown_tx: watch::Sender<bool>,
    shutdown_error: StdMutex<Option<String>>,
}

impl RuntimeSourceControl {
    fn new() -> Arc<Self> {
        let (shutdown_tx, _) = watch::channel(false);
        Arc::new(Self {
            pending: StdMutex::new(HashMap::new()),
            next_request_id: AtomicU64::new(1),
            shutdown_tx,
            shutdown_error: StdMutex::new(None),
        })
    }

    fn next_request_id(&self) -> u64 {
        self.next_request_id.fetch_add(1, Ordering::Relaxed)
    }

    fn register_request(
        &self,
        request_id: u64,
        response_tx: SyncSender<Result<RuntimeOffsetValue, String>>,
    ) {
        self.pending.lock().unwrap().insert(request_id, response_tx);
    }

    fn unregister_request(&self, request_id: u64) {
        self.pending.lock().unwrap().remove(&request_id);
    }

    fn resolve_response(&self, response: RuntimeOffsetRpcResponse) {
        let response_tx = {
            let mut pending = self.pending.lock().unwrap();
            pending.remove(&response.request_id)
        };
        if let Some(response_tx) = response_tx {
            let _ = response_tx.send(response.result);
        }
    }

    fn subscribe_shutdown(&self) -> watch::Receiver<bool> {
        self.shutdown_tx.subscribe()
    }

    fn shutdown(&self, error: Option<String>) {
        if let Some(error) = error {
            *self.shutdown_error.lock().unwrap() = Some(error);
        }
        let _ = self.shutdown_tx.send(true);
        let pending = std::mem::take(&mut *self.pending.lock().unwrap());
        for (_, response_tx) in pending {
            let _ = response_tx.send(Err(self
                .shutdown_error()
                .unwrap_or_else(|| "runtime source host closed control channel".to_string())));
        }
    }

    fn shutdown_error(&self) -> Option<String> {
        self.shutdown_error.lock().unwrap().clone()
    }
}

struct RuntimeSourceOffsetTransport {
    handle: Handle,
    control_writer: ControlWriter,
    control: Arc<RuntimeSourceControl>,
}

impl RuntimeSourceOffsetTransport {
    fn new(control_writer: ControlWriter, control: Arc<RuntimeSourceControl>) -> Self {
        Self {
            handle: Handle::current(),
            control_writer,
            control,
        }
    }

    async fn send_request(&self, request: RuntimeOffsetRpcRequest) -> io::Result<()> {
        self.control_writer
            .write(&PluginFrame::OffsetRequest(request))
            .await
    }
}

impl OffsetTransport for RuntimeSourceOffsetTransport {
    fn call(&self, operation: RuntimeOffsetOperation) -> Result<RuntimeOffsetValue, String> {
        let request_id = self.control.next_request_id();
        let (response_tx, response_rx) = std::sync::mpsc::sync_channel(1);
        self.control.register_request(request_id, response_tx);
        let request = RuntimeOffsetRpcRequest {
            request_id,
            operation,
        };
        if let Err(err) = block_on_handle(&self.handle, self.send_request(request)) {
            self.control.unregister_request(request_id);
            return Err(err.to_string());
        }
        response_rx.recv().map_err(|_| {
            self.control
                .shutdown_error()
                .unwrap_or_else(|| "runtime source response channel closed".to_string())
        })?
    }
}

struct RuntimeSourceCheckpointTransport {
    handle: Handle,
    data_writer: DataWriter,
}

impl RuntimeSourceCheckpointTransport {
    fn new(data_writer: DataWriter) -> Self {
        Self {
            handle: Handle::current(),
            data_writer,
        }
    }
}

impl CheckpointTransport for RuntimeSourceCheckpointTransport {
    fn store_checkpoint(&self, key: &str, envelope: &CheckpointEnvelope) -> Result<(), String> {
        let frame = PluginDataFrame::CheckpointUpdate {
            update: RuntimeCheckpointUpdate {
                key: key.to_string(),
                envelope: envelope.clone(),
            },
        };
        block_on_handle(&self.handle, async { self.data_writer.write(&frame).await })
            .map_err(|err| err.to_string())
    }
}

pub struct ArrowRelayToHostSink {
    control_writer: ControlWriter,
    data_writer: DataWriter,
    last_sent_schema_version: AtomicU64,
}

impl ArrowRelayToHostSink {
    fn new(control_writer: ControlWriter, data_writer: DataWriter) -> Self {
        Self {
            control_writer,
            data_writer,
            last_sent_schema_version: AtomicU64::new(0),
        }
    }

    async fn send_schema_state_if_needed(&self) -> io::Result<()> {
        let schema_state = current_runtime_schema_state_from_core();
        let last_sent = self.last_sent_schema_version.load(Ordering::Acquire);
        if schema_state.version <= last_sent {
            return Ok(());
        }
        self.control_writer
            .write(&PluginFrame::SourceEvent(SourceEvent::SchemaStateUpdate(
                schema_state.clone(),
            )))
            .await?;
        self.last_sent_schema_version
            .store(schema_state.version, Ordering::Release);
        Ok(())
    }
}

#[async_trait]
impl DataSink for ArrowRelayToHostSink {
    async fn sync(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
        cdc_ctx: Option<&SyncContext>,
    ) -> Result<(), io::Error> {
        self.send_schema_state_if_needed().await?;
        let arrow_stream_bytes = encode_record_batch_stream(stream).await?;
        self.data_writer
            .write(&PluginDataFrame::SinkWrite(RuntimeSourceSinkWrite {
                compaction_id: compaction_id_for_filename(&filename),
                filename,
                arrow_stream_bytes,
                cdc_ctx: cdc_ctx.cloned(),
            }))
            .await
    }

    fn runtime_ingest_relay(&self) -> Option<&dyn RuntimeIngestRelay> {
        Some(self)
    }
}

impl RuntimeIngestRelay for ArrowRelayToHostSink {
    fn relay_ingest_batches(
        &self,
        batches: Vec<RuntimeIngestPartitionBatch>,
    ) -> Result<(), std::io::Error> {
        block_on_handle(&self.control_writer.handle, async {
            self.send_schema_state_if_needed().await?;
            self.data_writer
                .write(&PluginDataFrame::IngestBatches { batches })
                .await
        })
    }

    fn relay_offset_hints(
        &self,
        offsets: Vec<RuntimeOffsetMaterializationHint>,
    ) -> Result<(), std::io::Error> {
        block_on_handle(&self.control_writer.handle, async {
            self.data_writer
                .write(&PluginDataFrame::OffsetMaterializationHints { hints: offsets })
                .await
        })
    }
}

async fn run_runtime_source_host_frame_loop(
    mut reader: OwnedReadHalf,
    control: Arc<RuntimeSourceControl>,
) -> io::Result<()> {
    loop {
        match read_frame_or_eof::<_, HostFrame>(&mut reader).await? {
            Some(HostFrame::OffsetResponse(response)) => control.resolve_response(response),
            Some(HostFrame::Shutdown) | None => {
                control.shutdown(None);
                return Ok(());
            }
            Some(other) => {
                let err = io::Error::other(format!(
                    "unexpected host control frame after source start: {:?}",
                    other
                ));
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

    {
        let mut current = PIPELINE_NAME.write();
        current.clear();
        current.push_str(&handshake.pipeline_name);
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
    Config::reset_envcache();
    Config::build_config();
    Config::init().await;

    let control = RuntimeSourceControl::new();
    let mut shutdown_rx = control.subscribe_shutdown();
    let reader_control = control.clone();
    tokio::spawn(async move {
        let _ = run_runtime_source_host_frame_loop(control_reader, reader_control).await;
    });

    let mut source = build(start).await?;
    let offsets = Arc::new(Offsets::from_runtime_transports(
        Arc::new(RuntimeSourceOffsetTransport::new(
            control_writer.clone(),
            control.clone(),
        )),
        Some(Arc::new(RuntimeSourceCheckpointTransport::new(
            data_writer.clone(),
        ))),
    ));
    let relay: Arc<Box<dyn DataSink + Send + Sync>> = Arc::new(Box::new(
        ArrowRelayToHostSink::new(control_writer.clone(), data_writer.clone()),
    ));

    let sync_result = {
        let sync_fut = source.sync(offsets.clone(), relay.clone());
        tokio::pin!(sync_fut);
        tokio::select! {
            result = &mut sync_fut => Some(result),
            result = wait_for_runtime_source_host_exit(&mut shutdown_rx, control.clone(), parent_pid) => match result {
                Ok(()) => None,
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
