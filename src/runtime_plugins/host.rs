use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use once_cell::sync::Lazy;
use serde::de::DeserializeOwned;
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tokio::time::{timeout, Duration};
use tracing::{info, warn};
use uuid::Uuid;
#[cfg(unix)]
use {
    nix::sys::signal::{kill, Signal},
    nix::unistd::Pid,
};

use crate::buffer::ingest_buffer::{flush_all_segments, Buffers, IngestBufferBatch};
use crate::discover::OutputMetadata;
use crate::helpers::configuration::Config;
use crate::helpers::offsets::{
    OffsetTypes, Offsets, RuntimeOffsetOperation, RuntimeOffsetRpcRequest,
    RuntimeOffsetRpcResponse, RuntimeOffsetValue,
};
use crate::plugins::cdc;
use crate::plugins::{DataSink, SchemaSink};
use crate::runtime_plugins::artifact::resolve_plugin_executable;
use crate::runtime_plugins::manifest::RuntimePluginManifest;
use crate::runtime_plugins::protocol::{
    HandshakeRequest, HostDataFrame, HostFrame, PluginDataFrame, PluginFrame, RuntimeBinding,
    RuntimeCheckpointUpdate, RuntimeExecutionContext, RuntimeOffsetMaterializationHint,
    RuntimeOutputLayout, RuntimeRequestAck, RuntimeSchemaConfig, RuntimeSchemaInstallRequest,
    RuntimeSchemaStateInstallRequest, RuntimeSessionHello, RuntimeSinkConfig,
    RuntimeSinkInstallRequest, RuntimeSinkPayload, RuntimeSourceConfig, SchemaRunRequest,
    SinkRunRequest, SourceEvent, SourceStartRequest, RUNTIME_PROTOCOL_VERSION,
    SKIPPR_RUNTIME_CONTROL_ADDR_ENV, SKIPPR_RUNTIME_DATA_ADDR_ENV,
    SKIPPR_RUNTIME_SESSION_TOKEN_ENV,
};
use crate::runtime_plugins::schema_state::{
    apply_runtime_source_schema_state, current_pipeline_schema_version,
    current_runtime_schema_state,
};
use crate::runtime_plugins::sdk::{decode_record_batch_stream, encode_record_batch_stream};
use crate::runtime_plugins::wire::{read_frame, write_frame, MAX_RUNTIME_FRAME_BYTES};

#[derive(Clone, Debug)]
pub struct ResolvedRuntimePlugin {
    pub manifest_path: PathBuf,
    pub manifest: RuntimePluginManifest,
}

impl ResolvedRuntimePlugin {
    pub fn load(manifest_path: impl AsRef<Path>) -> io::Result<Self> {
        let manifest_path = manifest_path.as_ref().to_path_buf();
        let manifest = RuntimePluginManifest::load_from_path(&manifest_path)?;
        Ok(Self {
            manifest_path,
            manifest,
        })
    }
}

struct RuntimeChildConnection {
    resolved: ResolvedRuntimePlugin,
    pipeline_name: String,
    child: Child,
    control: TcpStream,
    data: TcpStream,
}

static RUNTIME_PLUGIN_CHILD_PIDS: Lazy<std::sync::Mutex<BTreeSet<u32>>> =
    Lazy::new(|| std::sync::Mutex::new(BTreeSet::new()));
static RUNTIME_REQUEST_IDS: Lazy<std::sync::atomic::AtomicU64> =
    Lazy::new(|| std::sync::atomic::AtomicU64::new(1));

fn register_runtime_plugin_child(child_pid: Option<u32>) {
    if let Some(child_pid) = child_pid {
        RUNTIME_PLUGIN_CHILD_PIDS
            .lock()
            .expect("runtime plugin child registry poisoned")
            .insert(child_pid);
    }
}

fn unregister_runtime_plugin_child(child_pid: Option<u32>) {
    if let Some(child_pid) = child_pid {
        RUNTIME_PLUGIN_CHILD_PIDS
            .lock()
            .expect("runtime plugin child registry poisoned")
            .remove(&child_pid);
    }
}

#[cfg(unix)]
fn terminate_runtime_plugin_child_pid(child_pid: u32) {
    let _ = kill(Pid::from_raw(child_pid as i32), Signal::SIGKILL);
}

#[cfg(not(unix))]
fn terminate_runtime_plugin_child_pid(_child_pid: u32) {}

pub fn terminate_runtime_plugin_children() {
    let child_pids = {
        let registry = RUNTIME_PLUGIN_CHILD_PIDS
            .lock()
            .expect("runtime plugin child registry poisoned");
        registry.iter().copied().collect::<Vec<_>>()
    };
    for child_pid in &child_pids {
        terminate_runtime_plugin_child_pid(*child_pid);
    }
    let mut registry = RUNTIME_PLUGIN_CHILD_PIDS
        .lock()
        .expect("runtime plugin child registry poisoned");
    for child_pid in child_pids {
        registry.remove(&child_pid);
    }
}

fn next_runtime_request_id() -> u64 {
    RUNTIME_REQUEST_IDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

fn runtime_compaction_id(filename: &str) -> String {
    filename
        .rsplit_once("-c=")
        .map(|(_, suffix)| suffix.to_string())
        .unwrap_or_else(|| filename.to_string())
}

fn runtime_schema_compaction_id(
    binding: RuntimeBinding,
    schema_version: u64,
    namespace: &str,
) -> String {
    let binding = match binding {
        RuntimeBinding::Primary => "primary",
        RuntimeBinding::Deadletter => "deadletter",
    };
    format!("schema:{binding}:v{schema_version}:{namespace}")
}

impl RuntimeChildConnection {
    async fn accept_runtime_channel(listener: &TcpListener, token: &str) -> io::Result<TcpStream> {
        let (mut stream, _) = listener.accept().await?;
        let hello: RuntimeSessionHello = read_frame(&mut stream).await?;
        if hello.protocol_version != RUNTIME_PROTOCOL_VERSION {
            return Err(io::Error::other(format!(
                "runtime plugin session protocol mismatch: host={} child={}",
                RUNTIME_PROTOCOL_VERSION, hello.protocol_version
            )));
        }
        if hello.token != token {
            return Err(io::Error::other("runtime plugin session token mismatch"));
        }
        Ok(stream)
    }

    async fn spawn(resolved: ResolvedRuntimePlugin, pipeline_name: String) -> io::Result<Self> {
        let executable =
            resolve_plugin_executable(&resolved.manifest_path, &resolved.manifest).await?;
        let control_listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let data_listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let control_addr = control_listener.local_addr()?;
        let data_addr = data_listener.local_addr()?;
        let session_token = Uuid::new_v4().to_string();
        let mut command = Command::new(executable);
        command.kill_on_drop(true);
        #[cfg(target_os = "linux")]
        unsafe {
            let parent_pid = std::process::id() as libc::pid_t;
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() != parent_pid {
                    return Err(io::Error::other(
                        "runtime plugin parent exited before child initialization",
                    ));
                }
                Ok(())
            });
        }
        command
            .args(&resolved.manifest.args)
            .env("PIPELINE_NAME", &pipeline_name)
            .env("WORKSPACE_NAME", Config::get_workspace_name())
            .env("DATA_DIR", Config::get_pipeline_data_dir())
            .env(SKIPPR_RUNTIME_CONTROL_ADDR_ENV, control_addr.to_string())
            .env(SKIPPR_RUNTIME_DATA_ADDR_ENV, data_addr.to_string())
            .env(SKIPPR_RUNTIME_SESSION_TOKEN_ENV, &session_token)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());

        let child = command.spawn()?;
        register_runtime_plugin_child(child.id());
        let control = timeout(
            Duration::from_secs(15),
            Self::accept_runtime_channel(&control_listener, &session_token),
        )
        .await
        .map_err(|_| io::Error::other("timed out waiting for runtime control channel"))??;
        let data = timeout(
            Duration::from_secs(15),
            Self::accept_runtime_channel(&data_listener, &session_token),
        )
        .await
        .map_err(|_| io::Error::other("timed out waiting for runtime data channel"))??;
        let mut connection = Self {
            resolved,
            pipeline_name,
            child,
            control,
            data,
        };
        if let Err(err) = connection.handshake().await {
            let _ = connection.child.kill().await;
            unregister_runtime_plugin_child(connection.child.id());
            return Err(err);
        }
        Ok(connection)
    }

    async fn restart(&mut self) -> io::Result<()> {
        let child_pid = self.child.id();
        let _ = self.child.kill().await;
        unregister_runtime_plugin_child(child_pid);
        let replacement = Self::spawn(self.resolved.clone(), self.pipeline_name.clone()).await?;
        *self = replacement;
        Ok(())
    }

    async fn handshake(&mut self) -> io::Result<()> {
        let request = HostFrame::Handshake(HandshakeRequest {
            pipeline_name: self.pipeline_name.clone(),
            protocol_version: RUNTIME_PROTOCOL_VERSION,
        });
        write_frame(&mut self.control, &request).await?;
        let frame: PluginFrame = read_frame(&mut self.control).await?;
        let PluginFrame::HandshakeAck(handshake) = frame else {
            return Err(io::Error::other(
                "runtime plugin did not respond with a handshake ack",
            ));
        };
        self.resolved.manifest.verify_handshake(&handshake)
    }

    fn has_exited(&mut self) -> io::Result<bool> {
        let child_pid = self.child.id();
        let exited = self.child.try_wait()?.is_some();
        if exited {
            unregister_runtime_plugin_child(child_pid);
        }
        Ok(exited)
    }

    async fn send(&mut self, frame: &HostFrame) -> io::Result<()> {
        write_frame(&mut self.control, frame).await
    }

    async fn recv(&mut self) -> io::Result<PluginFrame> {
        read_frame(&mut self.control).await
    }

    async fn send_data(&mut self, frame: &HostDataFrame) -> io::Result<()> {
        write_frame(&mut self.data, frame).await
    }
}

impl Drop for RuntimeChildConnection {
    fn drop(&mut self) {
        unregister_runtime_plugin_child(self.child.id());
    }
}

fn store_checkpoint_update(offsets: &Offsets, update: &RuntimeCheckpointUpdate) -> io::Result<()> {
    offsets
        .store_checkpoint_envelope(&update.key, &update.envelope)
        .map_err(io::Error::other)
}

fn handle_runtime_offset_request(
    offsets: &Offsets,
    request: RuntimeOffsetRpcRequest,
) -> RuntimeOffsetRpcResponse {
    let result = match request.operation {
        RuntimeOffsetOperation::Validate {
            key,
            offset_type,
            offset_value,
        } => Ok(RuntimeOffsetValue::Validate(offsets.validate(
            &key,
            offset_type,
            offset_value,
        ))),
        RuntimeOffsetOperation::LoadCheckpointEnvelope { key } => Ok(
            RuntimeOffsetValue::LoadCheckpointEnvelope(offsets.load_checkpoint_envelope(&key)),
        ),
    };

    RuntimeOffsetRpcResponse {
        request_id: request.request_id,
        result,
    }
}

fn materialize_runtime_offset_hints(
    offsets: &Offsets,
    hints: Vec<RuntimeOffsetMaterializationHint>,
) {
    for hint in hints {
        offsets.set(&hint.key, OffsetTypes::Position, hint.position);
        if hint.closed {
            offsets.set(&hint.key, OffsetTypes::Closed, 1);
        }
    }
}

struct BufferedRuntimeFrameReader {
    buffer: Vec<u8>,
    consumed: usize,
    eof: bool,
}

impl BufferedRuntimeFrameReader {
    fn new() -> Self {
        Self {
            buffer: Vec::new(),
            consumed: 0,
            eof: false,
        }
    }

    fn is_drained(&self) -> bool {
        self.eof && self.available_bytes() == 0
    }

    fn available_bytes(&self) -> usize {
        self.buffer.len().saturating_sub(self.consumed)
    }

    fn fill_from_ready(&mut self, stream: &TcpStream) -> io::Result<()> {
        let mut scratch = [0u8; 64 * 1024];
        loop {
            match stream.try_read(&mut scratch) {
                Ok(0) => {
                    self.eof = true;
                    return Ok(());
                }
                Ok(read) => self.buffer.extend_from_slice(&scratch[..read]),
                Err(err) if err.kind() == ErrorKind::WouldBlock => return Ok(()),
                Err(err) => return Err(err),
            }
        }
    }

    fn take_frame<T>(&mut self) -> io::Result<Option<T>>
    where
        T: DeserializeOwned,
    {
        let available = self.available_bytes();
        if available < 4 {
            if self.eof && available > 0 {
                return Err(io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "runtime frame truncated while reading length prefix",
                ));
            }
            return Ok(None);
        }

        let prefix = <[u8; 4]>::try_from(&self.buffer[self.consumed..self.consumed + 4]).unwrap();
        let frame_len = u32::from_le_bytes(prefix) as usize;
        if frame_len > MAX_RUNTIME_FRAME_BYTES {
            return Err(io::Error::other(format!(
                "runtime frame length {} exceeds limit {}",
                frame_len, MAX_RUNTIME_FRAME_BYTES
            )));
        }

        let total_frame_len = 4 + frame_len;
        if available < total_frame_len {
            if self.eof {
                return Err(io::Error::new(
                    ErrorKind::UnexpectedEof,
                    format!(
                        "runtime frame truncated while reading payload (expected {} bytes, buffered {})",
                        frame_len,
                        available.saturating_sub(4)
                    ),
                ));
            }
            return Ok(None);
        }

        let payload_start = self.consumed + 4;
        let payload_end = payload_start + frame_len;
        let frame = bincode::deserialize(&self.buffer[payload_start..payload_end])
            .map_err(|err| io::Error::other(err.to_string()))?;
        self.consumed = payload_end;
        self.compact();
        Ok(Some(frame))
    }

    fn compact(&mut self) {
        if self.consumed == self.buffer.len() {
            self.buffer.clear();
            self.consumed = 0;
        } else if self.consumed >= 64 * 1024 {
            self.buffer.drain(0..self.consumed);
            self.consumed = 0;
        }
    }
}

fn build_source_start_request_for_pipeline(pipeline_name: &str) -> io::Result<SourceStartRequest> {
    let source_config = RuntimeSourceConfig::try_from(
        Config::get_pipeline_input_plugin_config().map_err(io::Error::other)?,
    )
    .map_err(io::Error::other)?;

    Ok(SourceStartRequest {
        context: runtime_execution_context(pipeline_name),
        config: source_config,
    })
}

async fn ingest_runtime_batches_into_core(
    batches: Vec<crate::runtime_plugins::protocol::RuntimeIngestPartitionBatch>,
    offsets: Arc<Offsets>,
    shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
) -> io::Result<()> {
    if batches.is_empty() {
        return Ok(());
    }

    let mut buffer_batches = Vec::with_capacity(batches.len());
    for batch in batches {
        let mut stream = decode_record_batch_stream(batch.arrow_stream_bytes)?;
        let mut record_batches = Vec::new();
        while let Some(next_batch) = stream.next().await {
            record_batches.push(next_batch.map_err(|err| io::Error::other(err.to_string()))?);
        }
        let schema = record_batches
            .first()
            .map(|batch| batch.schema())
            .unwrap_or_else(|| Arc::new(arrow::datatypes::Schema::empty()));
        let offsets_map = batch
            .offsets
            .into_iter()
            .map(|offset| (offset.key, offset.position))
            .collect();
        buffer_batches.push(IngestBufferBatch {
            offsets: offsets_map,
            sink_ref: batch.sink_ref,
            _namespace: batch.namespace,
            _partition: batch.partition,
            _time: batch.time,
            _shard: batch.shard,
            schema,
            record_batches: Some(record_batches),
            cdc_rows: batch.cdc_rows,
        });
    }

    let mut buffers = Buffers::new();
    buffers.write(buffer_batches);
    buffers
        .flush(offsets, shared_output)
        .await
        .map_err(|err| io::Error::other(err.to_string()))
}

async fn drain_runtime_source_tasks(pending_tasks: &mut JoinSet<io::Result<()>>) -> io::Result<()> {
    while let Some(joined) = pending_tasks.join_next().await {
        match joined {
            Ok(Ok(())) => {}
            Ok(Err(err)) => return Err(err),
            Err(err) => {
                return Err(io::Error::other(format!(
                    "runtime source task failed: {err}"
                )))
            }
        }
    }
    Ok(())
}

pub async fn sync_runtime_input_plugin(
    resolved: ResolvedRuntimePlugin,
    pipeline_name: String,
    offsets: Arc<Offsets>,
    shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
) -> io::Result<()> {
    let mut connection = RuntimeChildConnection::spawn(resolved, pipeline_name).await?;
    let start_request = build_source_start_request_for_pipeline(&connection.pipeline_name)?;
    connection
        .send(&HostFrame::RunSource(start_request))
        .await?;

    let mut control_completed = false;
    let mut data_completed = false;
    let mut saw_unflushed_batches = false;
    let mut pending_source_tasks: JoinSet<io::Result<()>> = JoinSet::new();
    let mut control_reader = BufferedRuntimeFrameReader::new();
    let mut data_reader = BufferedRuntimeFrameReader::new();
    loop {
        if !control_completed {
            if let Some(control_frame) = control_reader.take_frame::<PluginFrame>()? {
                match control_frame {
                    PluginFrame::SourceEvent(SourceEvent::SchemaStateUpdate(schema_state)) => {
                        apply_runtime_source_schema_state(schema_state.clone());
                        for namespace in schema_state.namespaces.keys() {
                            Config::sync_output_schema_namespace(namespace);
                        }
                    }
                    PluginFrame::SourceEvent(SourceEvent::Completed) => {
                        control_completed = true;
                        drain_runtime_source_tasks(&mut pending_source_tasks).await?;
                    }
                    PluginFrame::OffsetRequest(request) => {
                        let response = handle_runtime_offset_request(&offsets, request);
                        connection
                            .send(&HostFrame::OffsetResponse(response))
                            .await?;
                    }
                    PluginFrame::Error(err) => {
                        pending_source_tasks.abort_all();
                        return Err(io::Error::other(err));
                    }
                    other => {
                        pending_source_tasks.abort_all();
                        return Err(io::Error::other(format!(
                            "unexpected runtime source control frame: {:?}",
                            other
                        )));
                    }
                }
                continue;
            }
            if control_reader.is_drained() {
                pending_source_tasks.abort_all();
                return Err(io::Error::other(
                    "runtime source closed control channel before sending completion",
                ));
            }
        }

        if let Some(data_frame) = data_reader.take_frame::<PluginDataFrame>()? {
            match data_frame {
                PluginDataFrame::IngestBatches { batches } => {
                    if !batches.is_empty() {
                        if Config::debug_enabled() || Config::log_wal_enabled() {
                            let total_bytes: usize = batches
                                .iter()
                                .map(|batch| batch.arrow_stream_bytes.len())
                                .sum();
                            let offset_sample: Vec<String> = batches
                                .iter()
                                .take(3)
                                .map(|batch| {
                                    format!(
                                        "{}/{}@{}",
                                        batch.namespace,
                                        batch.partition,
                                        batch.arrow_stream_bytes.len()
                                    )
                                })
                                .collect();
                            info!(
                                "runtime source host: received {} prepared partition batches ({} bytes) sample={:?}",
                                batches.len(),
                                total_bytes,
                                offset_sample
                            );
                        }
                        let offsets = offsets.clone();
                        let shared_output = shared_output.clone();
                        pending_source_tasks.spawn_blocking(move || {
                            futures::executor::block_on(ingest_runtime_batches_into_core(
                                batches,
                                offsets,
                                shared_output,
                            ))
                        });
                        saw_unflushed_batches = true;
                    }
                }
                PluginDataFrame::SinkWrite(write) => {
                    let shared_output = shared_output.clone();
                    pending_source_tasks.spawn(async move {
                        let stream = decode_record_batch_stream(write.arrow_stream_bytes)?;
                        shared_output
                            .sync(stream, write.filename, write.cdc_ctx.as_ref())
                            .await?;
                        Ok(())
                    });
                }
                PluginDataFrame::CheckpointUpdate { update } => {
                    drain_runtime_source_tasks(&mut pending_source_tasks).await?;
                    if Config::debug_enabled() || Config::log_wal_enabled() {
                        info!(
                            "runtime source host: checkpoint update key={} authority={:?} saw_unflushed_batches={}",
                            update.key,
                            update.envelope.authority,
                            saw_unflushed_batches
                        );
                    }
                    if saw_unflushed_batches {
                        flush_all_segments(offsets.clone())
                            .await
                            .map_err(|err| io::Error::other(err.to_string()))?;
                        saw_unflushed_batches = false;
                    }
                    store_checkpoint_update(&offsets, &update)?;
                }
                PluginDataFrame::OffsetMaterializationHints { hints } => {
                    drain_runtime_source_tasks(&mut pending_source_tasks).await?;
                    if saw_unflushed_batches {
                        flush_all_segments(offsets.clone())
                            .await
                            .map_err(|err| io::Error::other(err.to_string()))?;
                        saw_unflushed_batches = false;
                    }
                    materialize_runtime_offset_hints(&offsets, hints);
                }
            }
            continue;
        }

        if !data_completed && data_reader.is_drained() {
            data_completed = true;
        }

        if control_completed && data_completed {
            drain_runtime_source_tasks(&mut pending_source_tasks).await?;
            if saw_unflushed_batches {
                flush_all_segments(offsets.clone())
                    .await
                    .map_err(|err| io::Error::other(err.to_string()))?;
            }
            break;
        }

        tokio::select! {
            control_ready = connection.control.readable(), if !control_completed => {
                control_ready?;
                control_reader.fill_from_ready(&connection.control)?;
            }
            data_ready = connection.data.readable(), if !data_completed => {
                data_ready?;
                data_reader.fill_from_ready(&connection.data)?;
            }
        }
    }

    drain_runtime_source_tasks(&mut pending_source_tasks).await?;
    Ok(())
}

fn runtime_execution_context(pipeline_name: &str) -> RuntimeExecutionContext {
    let partition_fields = Config::get_transform_batch_partition_fields()
        .split(',')
        .map(str::trim)
        .filter(|field| !field.is_empty())
        .map(|field| field.to_string())
        .collect();
    let order_fields = Config::get_transform_batch_order_fields()
        .split(',')
        .map(str::trim)
        .filter(|field| !field.is_empty())
        .map(|field| field.to_string())
        .collect();
    let time_partition_granularity = match Config::get_transform_batch_time_unit() {
        value if value.is_empty() => None,
        value => Some(value),
    };

    RuntimeExecutionContext {
        pipeline_name: pipeline_name.to_string(),
        workspace_name: Config::get_workspace_name(),
        data_dir: Config::get_data_dir(),
        output_layout: RuntimeOutputLayout {
            partition_fields,
            order_fields,
            time_partition_granularity,
        },
    }
}

async fn expect_install_ack(
    connection: &mut RuntimeChildConnection,
    plugin_type: &str,
) -> io::Result<()> {
    match connection.recv().await? {
        PluginFrame::Installed => Ok(()),
        PluginFrame::Error(err) => Err(io::Error::other(err)),
        other => Err(io::Error::other(format!(
            "unexpected runtime {plugin_type} install frame: {:?}",
            other
        ))),
    }
}

fn should_retry_runtime_connection(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        ErrorKind::BrokenPipe
            | ErrorKind::ConnectionAborted
            | ErrorKind::ConnectionReset
            | ErrorKind::NotConnected
            | ErrorKind::UnexpectedEof
    )
}

pub struct RuntimeDataSinkPlugin {
    install_request: RuntimeSinkInstallRequest,
    connection: Mutex<RuntimeChildConnection>,
}

impl RuntimeDataSinkPlugin {
    pub async fn new(
        resolved: ResolvedRuntimePlugin,
        pipeline_name: String,
        binding: RuntimeBinding,
        config: RuntimeSinkConfig,
    ) -> io::Result<Self> {
        let install_request = RuntimeSinkInstallRequest {
            context: runtime_execution_context(&pipeline_name),
            binding,
            config,
        };
        let connection = RuntimeChildConnection::spawn(resolved, pipeline_name).await?;
        let plugin = Self {
            install_request,
            connection: Mutex::new(connection),
        };
        {
            let mut guard = plugin.connection.lock().await;
            match plugin.install_runtime_state(&mut guard).await {
                Ok(()) => {}
                Err(err) if guard.has_exited()? || should_retry_runtime_connection(&err) => {
                    warn!(
                        "runtime sink startup install failed, restarting child: {}",
                        err
                    );
                    plugin.restart_and_reinstall(&mut guard).await?;
                }
                Err(err) => return Err(err),
            }
        }
        Ok(plugin)
    }

    async fn install_runtime_state(
        &self,
        connection: &mut RuntimeChildConnection,
    ) -> io::Result<()> {
        self.install_sink_binding(connection).await?;
        self.install_latest_schema_state(connection).await
    }

    async fn restart_and_reinstall(
        &self,
        connection: &mut RuntimeChildConnection,
    ) -> io::Result<()> {
        connection.restart().await?;
        self.install_runtime_state(connection).await
    }

    async fn ensure_connection_ready(
        &self,
        connection: &mut RuntimeChildConnection,
    ) -> io::Result<()> {
        if connection.has_exited()? {
            self.restart_and_reinstall(connection).await?;
        }
        Ok(())
    }

    async fn install_sink_binding(
        &self,
        connection: &mut RuntimeChildConnection,
    ) -> io::Result<()> {
        connection
            .send(&HostFrame::InstallSink(self.install_request.clone()))
            .await?;
        expect_install_ack(connection, "sink").await
    }

    async fn send_schema_state_install(
        &self,
        connection: &mut RuntimeChildConnection,
        schema_version: u64,
        namespaces: &BTreeMap<String, OutputMetadata>,
    ) -> io::Result<()> {
        connection
            .send(&HostFrame::InstallSchemaState(
                RuntimeSchemaStateInstallRequest {
                    schema_state: crate::runtime_plugins::protocol::RuntimeSchemaState {
                        version: schema_version,
                        namespaces: namespaces.clone(),
                    },
                },
            ))
            .await?;
        expect_install_ack(connection, "schema state").await
    }

    async fn install_latest_schema_state(
        &self,
        connection: &mut RuntimeChildConnection,
    ) -> io::Result<()> {
        let schema_state = current_runtime_schema_state();
        self.send_schema_state_install(connection, schema_state.version, &schema_state.namespaces)
            .await
    }

    async fn send_sink_request(
        &self,
        request: SinkRunRequest,
        arrow_stream_bytes: Vec<u8>,
    ) -> io::Result<()> {
        let mut retried = false;
        let mut schema_refreshes = 0usize;
        loop {
            let mut guard = self.connection.lock().await;
            self.ensure_connection_ready(&mut guard).await?;

            let send_result = guard.send(&HostFrame::RunSink(request.clone())).await;
            let recv_result = match send_result {
                Ok(_) => {
                    let payload = HostDataFrame::SinkPayload(RuntimeSinkPayload {
                        request_id: request.request_id,
                        arrow_stream_bytes: arrow_stream_bytes.clone(),
                    });
                    match guard.send_data(&payload).await {
                        Ok(()) => guard.recv().await,
                        Err(err) => Err(err),
                    }
                }
                Err(err) => Err(err),
            };

            match recv_result {
                Ok(PluginFrame::SinkAck(RuntimeRequestAck { request_id }))
                    if request_id == request.request_id =>
                {
                    return Ok(())
                }
                Ok(PluginFrame::Error(err)) => return Err(io::Error::other(err)),
                Ok(PluginFrame::SchemaStateRefreshRequired(_refresh)) => {
                    if schema_refreshes >= 3 {
                        return Err(io::Error::other(
                            "runtime sink repeatedly requested schema refresh",
                        ));
                    }
                    self.install_latest_schema_state(&mut guard).await?;
                    schema_refreshes += 1;
                    continue;
                }
                Ok(other) => {
                    return Err(io::Error::other(format!(
                        "unexpected runtime sink frame: {:?}",
                        other
                    )))
                }
                Err(err)
                    if !retried
                        && (guard.has_exited()? || should_retry_runtime_connection(&err)) =>
                {
                    warn!("runtime sink request failed, restarting child: {}", err);
                    self.restart_and_reinstall(&mut guard).await?;
                    retried = true;
                    continue;
                }
                Err(err) => return Err(err),
            }
        }
    }

    async fn install_schema_state_with_retry(
        &self,
        schema_version: u64,
        namespaces: &BTreeMap<String, OutputMetadata>,
    ) -> io::Result<()> {
        let mut retried = false;
        loop {
            let mut guard = self.connection.lock().await;
            self.ensure_connection_ready(&mut guard).await?;
            match self
                .send_schema_state_install(&mut guard, schema_version, namespaces)
                .await
            {
                Ok(()) => return Ok(()),
                Err(err)
                    if !retried
                        && (guard.has_exited()? || should_retry_runtime_connection(&err)) =>
                {
                    warn!(
                        "runtime sink schema state install failed, restarting child: {}",
                        err
                    );
                    self.restart_and_reinstall(&mut guard).await?;
                    retried = true;
                    continue;
                }
                Err(err) => return Err(err),
            }
        }
    }
}

#[async_trait]
impl DataSink for RuntimeDataSinkPlugin {
    async fn sync(
        &self,
        stream: datafusion::execution::SendableRecordBatchStream,
        filename: String,
        cdc_ctx: Option<&cdc::SyncContext>,
    ) -> Result<(), io::Error> {
        let arrow_stream_bytes = encode_record_batch_stream(stream).await?;
        let request = SinkRunRequest {
            request_id: next_runtime_request_id(),
            compaction_id: runtime_compaction_id(&filename),
            binding: self.install_request.binding,
            required_schema_version: current_pipeline_schema_version(),
            filename,
            cdc_ctx: cdc_ctx.cloned(),
        };
        self.send_sink_request(request, arrow_stream_bytes).await
    }

    fn capability(&self) -> Option<&'static cdc::SinkCapability> {
        None
    }

    async fn install_schema_state(
        &self,
        schema_version: u64,
        namespaces: &BTreeMap<String, OutputMetadata>,
    ) -> Result<(), io::Error> {
        self.install_schema_state_with_retry(schema_version, namespaces)
            .await
    }
}

pub struct RuntimeSchemaSinkPlugin {
    install_request: RuntimeSchemaInstallRequest,
    connection: Mutex<RuntimeChildConnection>,
}

impl RuntimeSchemaSinkPlugin {
    pub async fn new(
        resolved: ResolvedRuntimePlugin,
        pipeline_name: String,
        binding: RuntimeBinding,
        config: RuntimeSchemaConfig,
    ) -> io::Result<Self> {
        let install_request = RuntimeSchemaInstallRequest {
            context: runtime_execution_context(&pipeline_name),
            binding,
            config,
        };
        let connection = RuntimeChildConnection::spawn(resolved, pipeline_name).await?;
        let plugin = Self {
            install_request,
            connection: Mutex::new(connection),
        };
        {
            let mut guard = plugin.connection.lock().await;
            match plugin.install_runtime_state(&mut guard).await {
                Ok(()) => {}
                Err(err) if guard.has_exited()? || should_retry_runtime_connection(&err) => {
                    warn!(
                        "runtime schema sink startup install failed, restarting child: {}",
                        err
                    );
                    plugin.restart_and_reinstall(&mut guard).await?;
                }
                Err(err) => return Err(err),
            }
        }
        Ok(plugin)
    }

    async fn install_runtime_state(
        &self,
        connection: &mut RuntimeChildConnection,
    ) -> io::Result<()> {
        self.install_schema_binding(connection).await?;
        self.install_latest_schema_state(connection).await
    }

    async fn restart_and_reinstall(
        &self,
        connection: &mut RuntimeChildConnection,
    ) -> io::Result<()> {
        connection.restart().await?;
        self.install_runtime_state(connection).await
    }

    async fn ensure_connection_ready(
        &self,
        connection: &mut RuntimeChildConnection,
    ) -> io::Result<()> {
        if connection.has_exited()? {
            self.restart_and_reinstall(connection).await?;
        }
        Ok(())
    }

    async fn install_schema_binding(
        &self,
        connection: &mut RuntimeChildConnection,
    ) -> io::Result<()> {
        connection
            .send(&HostFrame::InstallSchema(self.install_request.clone()))
            .await?;
        expect_install_ack(connection, "schema").await
    }

    async fn send_schema_state_install(
        &self,
        connection: &mut RuntimeChildConnection,
        schema_version: u64,
        namespaces: &BTreeMap<String, OutputMetadata>,
    ) -> io::Result<()> {
        connection
            .send(&HostFrame::InstallSchemaState(
                RuntimeSchemaStateInstallRequest {
                    schema_state: crate::runtime_plugins::protocol::RuntimeSchemaState {
                        version: schema_version,
                        namespaces: namespaces.clone(),
                    },
                },
            ))
            .await?;
        expect_install_ack(connection, "schema state").await
    }

    async fn install_latest_schema_state(
        &self,
        connection: &mut RuntimeChildConnection,
    ) -> io::Result<()> {
        let schema_state = current_runtime_schema_state();
        self.send_schema_state_install(connection, schema_state.version, &schema_state.namespaces)
            .await
    }

    async fn send_schema_request(&self, request: SchemaRunRequest) -> io::Result<()> {
        let mut retried = false;
        let mut schema_refreshes = 0usize;
        loop {
            let mut guard = self.connection.lock().await;
            self.ensure_connection_ready(&mut guard).await?;

            let send_result = guard.send(&HostFrame::RunSchema(request.clone())).await;
            let recv_result = match send_result {
                Ok(_) => guard.recv().await,
                Err(err) => Err(err),
            };

            match recv_result {
                Ok(PluginFrame::SchemaAck(RuntimeRequestAck { request_id }))
                    if request_id == request.request_id =>
                {
                    return Ok(())
                }
                Ok(PluginFrame::Error(err)) => return Err(io::Error::other(err)),
                Ok(PluginFrame::SchemaStateRefreshRequired(_refresh)) => {
                    if schema_refreshes >= 3 {
                        return Err(io::Error::other(
                            "runtime schema sink repeatedly requested schema refresh",
                        ));
                    }
                    self.install_latest_schema_state(&mut guard).await?;
                    schema_refreshes += 1;
                    continue;
                }
                Ok(other) => {
                    return Err(io::Error::other(format!(
                        "unexpected runtime schema frame: {:?}",
                        other
                    )))
                }
                Err(err)
                    if !retried
                        && (guard.has_exited()? || should_retry_runtime_connection(&err)) =>
                {
                    warn!("runtime schema request failed, restarting child: {}", err);
                    self.restart_and_reinstall(&mut guard).await?;
                    retried = true;
                    continue;
                }
                Err(err) => return Err(err),
            }
        }
    }

    async fn install_schema_state_with_retry(
        &self,
        schema_version: u64,
        namespaces: &BTreeMap<String, OutputMetadata>,
    ) -> io::Result<()> {
        let mut retried = false;
        loop {
            let mut guard = self.connection.lock().await;
            self.ensure_connection_ready(&mut guard).await?;
            match self
                .send_schema_state_install(&mut guard, schema_version, namespaces)
                .await
            {
                Ok(()) => return Ok(()),
                Err(err)
                    if !retried
                        && (guard.has_exited()? || should_retry_runtime_connection(&err)) =>
                {
                    warn!(
                        "runtime schema sink schema state install failed, restarting child: {}",
                        err
                    );
                    self.restart_and_reinstall(&mut guard).await?;
                    retried = true;
                    continue;
                }
                Err(err) => return Err(err),
            }
        }
    }
}

#[async_trait]
impl SchemaSink for RuntimeSchemaSinkPlugin {
    async fn sync_schema(
        &self,
        namespace: &str,
        _metadata: &crate::discover::OutputMetadata,
    ) -> Result<(), io::Error> {
        let schema_version = current_pipeline_schema_version();
        let request = SchemaRunRequest {
            request_id: next_runtime_request_id(),
            compaction_id: runtime_schema_compaction_id(
                self.install_request.binding,
                schema_version,
                namespace,
            ),
            binding: self.install_request.binding,
            required_schema_version: schema_version,
            namespace: namespace.to_string(),
        };
        self.send_schema_request(request).await
    }

    async fn install_schema_state(
        &self,
        schema_version: u64,
        namespaces: &BTreeMap<String, OutputMetadata>,
    ) -> Result<(), io::Error> {
        self.install_schema_state_with_retry(schema_version, namespaces)
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, MutexGuard, OnceLock};

    use super::{
        handle_runtime_offset_request, BufferedRuntimeFrameReader, MAX_RUNTIME_FRAME_BYTES,
    };
    use crate::helpers::configuration::Config;
    use crate::helpers::offsets::{
        OffsetKey, OffsetTypes, Offsets, RuntimeOffsetOperation, RuntimeOffsetRpcRequest,
        RuntimeOffsetValue,
    };
    use crate::plugins::cdc::{CheckpointAuthority, CheckpointEnvelope, CheckpointKind};
    use serde::{Deserialize, Serialize};
    use serial_test::serial;
    use tempfile::TempDir;

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn env_lock() -> MutexGuard<'static, ()> {
        ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    struct DataDirGuard {
        old_data_dir: Option<String>,
        _temp_dir: TempDir,
        _lock: MutexGuard<'static, ()>,
    }

    impl Drop for DataDirGuard {
        fn drop(&mut self) {
            if let Some(ref value) = self.old_data_dir {
                Config::setenv("DATA_DIR", value);
            } else {
                std::env::remove_var("DATA_DIR");
                Config::set_evncache("DATA_DIR", "");
            }
            Config::reset_envcache();
        }
    }

    fn setup_offsets() -> (DataDirGuard, Offsets) {
        let lock = env_lock();
        let temp_dir = tempfile::tempdir().unwrap();
        let old_data_dir = std::env::var("DATA_DIR").ok();
        Config::setenv("DATA_DIR", temp_dir.path().to_str().unwrap());
        Config::reset_envcache();
        let offsets = Offsets::init().unwrap();
        offsets.clear_for_test();
        (
            DataDirGuard {
                old_data_dir,
                _temp_dir: temp_dir,
                _lock: lock,
            },
            offsets,
        )
    }

    #[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
    struct TestFrame {
        value: u32,
    }

    fn encode_test_frame(frame: &TestFrame) -> Vec<u8> {
        let payload = bincode::serialize(frame).unwrap();
        let mut bytes = (payload.len() as u32).to_le_bytes().to_vec();
        bytes.extend_from_slice(&payload);
        bytes
    }

    #[test]
    fn buffered_runtime_frame_reader_reassembles_split_frames() {
        let mut reader = BufferedRuntimeFrameReader::new();
        let bytes = encode_test_frame(&TestFrame { value: 7 });

        reader.buffer.extend_from_slice(&bytes[..2]);
        assert_eq!(reader.take_frame::<TestFrame>().unwrap(), None);

        reader.buffer.extend_from_slice(&bytes[2..6]);
        assert_eq!(reader.take_frame::<TestFrame>().unwrap(), None);

        reader.buffer.extend_from_slice(&bytes[6..]);
        assert_eq!(
            reader.take_frame::<TestFrame>().unwrap(),
            Some(TestFrame { value: 7 })
        );
        assert_eq!(reader.available_bytes(), 0);
    }

    #[test]
    fn buffered_runtime_frame_reader_rejects_oversized_lengths() {
        let mut reader = BufferedRuntimeFrameReader::new();
        reader
            .buffer
            .extend_from_slice(&((MAX_RUNTIME_FRAME_BYTES + 1) as u32).to_le_bytes());

        let err = reader.take_frame::<TestFrame>().unwrap_err();
        assert!(err.to_string().contains("exceeds limit"));
    }

    #[test]
    #[serial]
    fn runtime_offset_request_handler_reads_host_state() {
        let (_guard, offsets) = setup_offsets();
        let key = OffsetKey::new("runtime-host", "partition-1");

        let initial_validate = handle_runtime_offset_request(
            &offsets,
            RuntimeOffsetRpcRequest {
                request_id: 1,
                operation: RuntimeOffsetOperation::Validate {
                    key: key.clone(),
                    offset_type: OffsetTypes::Closed,
                    offset_value: 1,
                },
            },
        );
        assert_eq!(
            initial_validate.result.unwrap(),
            RuntimeOffsetValue::Validate(None)
        );

        offsets.set(&key, OffsetTypes::Closed, 1);
        offsets
            .store_checkpoint_payload(
                "runtime-host-checkpoint",
                CheckpointAuthority::AdvisoryHint,
                CheckpointKind::AdvisoryProgress,
                1,
                &b"checkpoint".to_vec(),
            )
            .unwrap();

        let validate_response = handle_runtime_offset_request(
            &offsets,
            RuntimeOffsetRpcRequest {
                request_id: 2,
                operation: RuntimeOffsetOperation::Validate {
                    key: key.clone(),
                    offset_type: OffsetTypes::Closed,
                    offset_value: 1,
                },
            },
        );
        assert_eq!(
            validate_response.result.unwrap(),
            RuntimeOffsetValue::Validate(Some(true))
        );

        let load_checkpoint = handle_runtime_offset_request(
            &offsets,
            RuntimeOffsetRpcRequest {
                request_id: 3,
                operation: RuntimeOffsetOperation::LoadCheckpointEnvelope {
                    key: "runtime-host-checkpoint".to_string(),
                },
            },
        );
        assert_eq!(
            load_checkpoint.result.unwrap(),
            RuntimeOffsetValue::LoadCheckpointEnvelope(Some(
                CheckpointEnvelope::from_payload(
                    CheckpointAuthority::AdvisoryHint,
                    CheckpointKind::AdvisoryProgress,
                    1,
                    &b"checkpoint".to_vec(),
                )
                .unwrap(),
            ))
        );
    }
}
