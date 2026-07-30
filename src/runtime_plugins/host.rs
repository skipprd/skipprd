use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Instant;

use arrow::datatypes::{DataType as ArrowDataType, Field as ArrowField, SchemaRef};
use async_trait::async_trait;
use futures::StreamExt;
use once_cell::sync::Lazy;
use serde::de::DeserializeOwned;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, Mutex, Notify};
use tokio::task::JoinSet;
use tokio::time::{timeout, Duration};
use tracing::{debug, info, warn};
use uuid::Uuid;
#[cfg(unix)]
use {
    nix::sys::signal::{kill, Signal},
    nix::unistd::Pid,
};

use crate::buffer::ingest_buffer::{flush_all_segments, IngestBufferBatch};
use crate::discover::{OutputMetadata, SkipprDataType};
use crate::helpers::configuration::Config;
use crate::helpers::offsets::{OffsetTypes, Offsets};
#[cfg(test)]
use crate::helpers::offsets::{
    RuntimeOffsetOperation, RuntimeOffsetRpcRequest, RuntimeOffsetRpcResponse, RuntimeOffsetValue,
};
use crate::ingest_work::{
    storage_namespace, storage_partition, Ingest, IngestBatch, IngestTask, IngestTasks, INGEST_RT,
};
use crate::plugins::cdc;
use crate::plugins::{DataSink, SchemaSink, SchemaSyncRequest, SinkWriteOutcome};
use crate::runtime_plugins::artifact::resolve_plugin_executable;
use crate::runtime_plugins::manifest::RuntimePluginManifest;
use crate::runtime_plugins::offset_service::OffsetServiceEndpoint;
use crate::runtime_plugins::protocol::{
    CommitReceipt, FinishSink, HandshakeRequest, HostDataFrame, HostFrame, PluginDataFrame,
    PluginFrame, PrepareAck, PrepareSink, PrepareSinkResult, RuntimeBinding,
    RuntimeCheckpointUpdate, RuntimeExecutionContext, RuntimeExecutionMode, RuntimeIngestAck,
    RuntimeOffsetMaterializationHint, RuntimeOutputLayout, RuntimeRequestAck, RuntimeSchemaConfig,
    RuntimeSchemaInstallRequest, RuntimeSchemaState, RuntimeSchemaStateInstallRequest,
    RuntimeSessionHello, RuntimeSinkConfig, RuntimeSinkError, RuntimeSinkInstallRequest,
    RuntimeSinkPayloadMode, RuntimeSourceConfig, RuntimeSourceIngestWindow, SchemaRunRequest,
    SinkAck, SinkChunk, SinkRunRequest, SourceEvent, SourceStartRequest, COMMIT_RECEIPT_VERSION,
    MAX_RUNTIME_SINK_CHUNK_BYTES, RUNTIME_PROTOCOL_VERSION, SKIPPR_RUNTIME_CONTROL_ADDR_ENV,
    SKIPPR_RUNTIME_DATA_ADDR_ENV, SKIPPR_RUNTIME_OFFSET_ADDR_ENV, SKIPPR_RUNTIME_SESSION_TOKEN_ENV,
};
use crate::runtime_plugins::schema_state::{
    apply_runtime_source_schema_state, bump_pipeline_schema_version,
    current_pipeline_schema_version, current_runtime_schema_state,
};
use crate::runtime_plugins::sdk::{
    decode_record_batch_stream, decode_record_batch_stream_with_stats,
    encode_record_batch_stream_with_stats, encode_record_batches,
};
use crate::runtime_plugins::wire::{read_frame, write_frame, MAX_RUNTIME_FRAME_BYTES};
use crate::sink_apply_identity::SinkApplyEnvelopeV2;

#[derive(Clone, Debug)]
pub struct ResolvedRuntimePlugin {
    pub manifest_path: PathBuf,
    pub manifest: RuntimePluginManifest,
}

impl ResolvedRuntimePlugin {
    pub fn load(manifest_path: impl AsRef<Path>) -> io::Result<Self> {
        let manifest_path = manifest_path.as_ref().to_path_buf();
        let manifest = RuntimePluginManifest::load_from_path(&manifest_path)?;
        if manifest.protocol_version != RUNTIME_PROTOCOL_VERSION {
            return Err(io::Error::other(format!(
                "runtime plugin manifest '{}' uses protocol {}, but host requires protocol {}",
                manifest.name, manifest.protocol_version, RUNTIME_PROTOCOL_VERSION
            )));
        }
        if manifest.kind == crate::runtime_plugins::protocol::RuntimePluginKind::DataSink {
            let capability = manifest.sink_capability.as_ref().ok_or_else(|| {
                io::Error::other(format!(
                    "runtime sink manifest '{}' is missing sink capability",
                    manifest.name
                ))
            })?;
            if capability.max_sessions_per_child == 0 {
                return Err(io::Error::other(format!(
                    "runtime sink manifest '{}' declares max_sessions_per_child=0",
                    manifest.name
                )));
            }
            if matches!(
                capability.grouping_support,
                crate::buffer::compaction_transaction::SinkGroupingSupport::None
            ) {
                return Err(io::Error::other(format!(
                    "runtime sink manifest '{}' declares grouping_support=None; grouped compaction requires a non-None grouping support",
                    manifest.name
                )));
            }
            if !capability.supports_bounded_grouped_stream {
                return Err(io::Error::other(format!(
                    "runtime sink manifest '{}' does not advertise bounded grouped streaming; grouped compaction requires a protocol-17 bounded grouped sink",
                    manifest.name
                )));
            }
        }
        Ok(Self {
            manifest_path,
            manifest,
        })
    }
}

struct RuntimePluginChild(Child);

impl std::ops::Deref for RuntimePluginChild {
    type Target = Child;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for RuntimePluginChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for RuntimePluginChild {
    fn drop(&mut self) {
        unregister_runtime_plugin_child(self.0.id());
    }
}

struct RuntimeChildConnection {
    resolved: ResolvedRuntimePlugin,
    pipeline_name: String,
    sink_session_capacity: usize,
    child: RuntimePluginChild,
    control: TcpStream,
    data: TcpStream,
    installed_schema_version: Option<u64>,
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
    schema_fingerprint: &str,
    namespace: &str,
) -> String {
    let binding = match binding {
        RuntimeBinding::Primary => "primary",
        RuntimeBinding::Deadletter => "deadletter",
    };
    format!("schema:{binding}:v{schema_fingerprint}:{namespace}")
}

fn runtime_schema_compaction_fingerprint(metadata: &OutputMetadata) -> String {
    let bytes =
        serde_json::to_vec(metadata).unwrap_or_else(|_| metadata.lineage_id().as_bytes().to_vec());
    format!("{:x}", md5::compute(bytes))
}

fn sink_apply_envelope(request: &SinkRunRequest) -> SinkApplyEnvelopeV2 {
    SinkApplyEnvelopeV2::new(
        request.compaction_id.clone(),
        request.idempotency_key.clone(),
        &request.wal_refs,
        request.cdc_ctx.is_some(),
        request
            .source_contract
            .as_ref()
            .map(|contract| contract.write_policy)
            .unwrap_or_default(),
        request.write_semantics,
        (!request.schema_fingerprint.is_empty()).then(|| request.schema_fingerprint.clone()),
        Some(request.required_schema_version),
        request.source_contract.clone(),
    )
}

fn validate_commit_receipt(
    receipt: &CommitReceipt,
    envelope: &SinkApplyEnvelopeV2,
) -> io::Result<()> {
    if receipt.version != COMMIT_RECEIPT_VERSION
        || receipt.compaction_id != envelope.compaction_id
        || receipt.idempotency_key != envelope.idempotency_key
        || receipt.wal_refs_fingerprint != envelope.wal_refs_fingerprint
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "runtime sink commit receipt does not match prepared apply envelope",
        ));
    }
    Ok(())
}

fn validate_sink_ack(ack: &SinkAck, envelope: &SinkApplyEnvelopeV2) -> io::Result<()> {
    validate_commit_receipt(&ack.receipt, envelope)?;
    if !ack.catalog_intents.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "runtime sink returned catalog intents before Glue outbox migration",
        ));
    }
    Ok(())
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

    async fn spawn(
        resolved: ResolvedRuntimePlugin,
        pipeline_name: String,
        offset_addr: Option<String>,
        session_token: Option<String>,
        sink_session_capacity: usize,
    ) -> io::Result<Self> {
        let executable =
            resolve_plugin_executable(&resolved.manifest_path, &resolved.manifest).await?;
        let control_listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let data_listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let control_addr = control_listener.local_addr()?;
        let data_addr = data_listener.local_addr()?;
        let session_token = session_token.unwrap_or_else(|| Uuid::new_v4().to_string());
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
            .env(SKIPPR_RUNTIME_SESSION_TOKEN_ENV, &session_token);
        if let Some(offset_addr) = offset_addr {
            command.env(SKIPPR_RUNTIME_OFFSET_ADDR_ENV, offset_addr);
        }
        command
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
            sink_session_capacity: sink_session_capacity.max(1),
            child: RuntimePluginChild(child),
            control,
            data,
            installed_schema_version: None,
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
        let replacement = Self::spawn(
            self.resolved.clone(),
            self.pipeline_name.clone(),
            None,
            None,
            self.sink_session_capacity,
        )
        .await?;
        *self = replacement;
        Ok(())
    }

    async fn handshake(&mut self) -> io::Result<()> {
        let request = HostFrame::Handshake(HandshakeRequest {
            pipeline_name: self.pipeline_name.clone(),
            protocol_version: RUNTIME_PROTOCOL_VERSION,
            sink_session_capacity: self.sink_session_capacity,
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

    fn try_exit_status(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        let status = self.child.try_wait()?;
        if status.is_some() {
            unregister_runtime_plugin_child(self.child.id());
        }
        Ok(status)
    }

    async fn send(&mut self, frame: &HostFrame) -> io::Result<()> {
        write_frame(&mut self.control, frame).await?;
        if matches!(
            frame,
            HostFrame::InstallSchemaState(_) | HostFrame::InstallSchemaDelta(_)
        ) {
            crate::metrics::counters::add_runtime_schema_state_install_sent(1);
        }
        Ok(())
    }

    async fn recv(&mut self) -> io::Result<PluginFrame> {
        read_frame(&mut self.control).await
    }
}

fn store_checkpoint_update(offsets: &Offsets, update: &RuntimeCheckpointUpdate) -> io::Result<()> {
    offsets
        .store_checkpoint_envelope(&update.key, &update.envelope)
        .map_err(io::Error::other)
}

#[cfg(test)]
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
                Err(err) if is_runtime_channel_eof(&err) => {
                    self.eof = true;
                    return Ok(());
                }
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

    fn take_frame_payload(&mut self) -> io::Result<Option<Vec<u8>>> {
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
                    "runtime frame truncated while reading payload",
                ));
            }
            return Ok(None);
        }

        let payload_start = self.consumed + 4;
        let payload_end = payload_start + frame_len;
        let payload = self.buffer[payload_start..payload_end].to_vec();
        self.consumed = payload_end;
        self.compact();
        Ok(Some(payload))
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

fn is_runtime_channel_eof(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        ErrorKind::ConnectionAborted | ErrorKind::ConnectionReset | ErrorKind::UnexpectedEof
    )
}

fn build_source_start_request_for_pipeline(
    pipeline_name: &str,
    execution_mode: RuntimeExecutionMode,
    source_once: bool,
) -> io::Result<SourceStartRequest> {
    let source_config = RuntimeSourceConfig::try_from(
        Config::get_pipeline_input_plugin_config().map_err(io::Error::other)?,
    )
    .map_err(io::Error::other)?;

    Ok(SourceStartRequest {
        context: runtime_execution_context(pipeline_name, execution_mode),
        config: source_config,
        once: source_once || execution_mode == RuntimeExecutionMode::Discover,
        source_ingest_window: runtime_source_ingest_window(),
    })
}

fn runtime_source_ingest_window() -> RuntimeSourceIngestWindow {
    let worker_cap = runtime_source_blocking_spawn_cap().max(1);
    let wal_cap = crate::buffer::wal_writer::queue_capacity().max(1);
    let default_max = worker_cap.saturating_add(wal_cap).clamp(2, 64);
    let max_in_flight_requests = std::env::var("RUNTIME_SOURCE_MAX_IN_FLIGHT_REQUESTS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value >= 2)
        .unwrap_or(default_max);
    RuntimeSourceIngestWindow {
        max_in_flight_requests,
        max_in_flight_bytes: 512 * 1024 * 1024,
    }
}

fn skippr_type_for_arrow(data_type: &ArrowDataType) -> SkipprDataType {
    match data_type {
        ArrowDataType::Boolean => SkipprDataType::Boolean,
        ArrowDataType::Int8 | ArrowDataType::UInt8 => SkipprDataType::Byte,
        ArrowDataType::Int16 | ArrowDataType::UInt16 => SkipprDataType::Short,
        ArrowDataType::Int32 | ArrowDataType::UInt32 => SkipprDataType::Integer,
        ArrowDataType::Int64 | ArrowDataType::UInt64 => SkipprDataType::Long,
        ArrowDataType::Float16 | ArrowDataType::Float32 => SkipprDataType::Float,
        ArrowDataType::Float64 => SkipprDataType::Double,
        ArrowDataType::Decimal128(_, _) | ArrowDataType::Decimal256(_, _) => {
            SkipprDataType::Decimal
        }
        ArrowDataType::Date32 | ArrowDataType::Date64 => SkipprDataType::Date,
        ArrowDataType::Timestamp(_, _) => SkipprDataType::Timestamp,
        ArrowDataType::Time32(_) | ArrowDataType::Time64(_) => SkipprDataType::Time,
        ArrowDataType::Binary
        | ArrowDataType::LargeBinary
        | ArrowDataType::FixedSizeBinary(_)
        | ArrowDataType::BinaryView => SkipprDataType::Binary,
        ArrowDataType::Utf8 | ArrowDataType::LargeUtf8 | ArrowDataType::Utf8View => {
            SkipprDataType::String
        }
        ArrowDataType::List(_)
        | ArrowDataType::LargeList(_)
        | ArrowDataType::FixedSizeList(_, _) => SkipprDataType::Array,
        ArrowDataType::Map(_, _) => SkipprDataType::Map,
        ArrowDataType::Struct(_) => SkipprDataType::Record,
        ArrowDataType::Null => SkipprDataType::Null,
        _ => SkipprDataType::String,
    }
}

fn output_metadata_for_arrow_field(field: &ArrowField) -> OutputMetadata {
    let mut metadata = OutputMetadata::new();
    metadata.out_field_name = field.name().clone();
    metadata.source_field_name = field.name().clone();
    metadata.determined_type = skippr_type_for_arrow(field.data_type());
    metadata.nullable = field.is_nullable();
    if let ArrowDataType::Struct(fields) = field.data_type() {
        metadata.fields = Box::new(
            fields
                .iter()
                .map(|child| (child.name().clone(), output_metadata_for_arrow_field(child)))
                .collect::<HashMap<_, _>>(),
        );
    }
    metadata
}

fn output_metadata_for_arrow_schema(schema: &SchemaRef) -> OutputMetadata {
    let mut metadata = OutputMetadata::new();
    metadata.determined_type = SkipprDataType::Record;
    metadata.fields = Box::new(
        schema
            .fields()
            .iter()
            .map(|field| (field.name().clone(), output_metadata_for_arrow_field(field)))
            .collect::<HashMap<_, _>>(),
    );
    metadata
}

fn query_value_from_runtime_filename(filename: &str, key: &str) -> Option<String> {
    filename.split('&').find_map(|part| {
        let (part_key, part_value) = part.split_once('=')?;
        (part_key == key).then(|| part_value.to_string())
    })
}

fn apply_derived_runtime_schema(namespace: String, schema: &SchemaRef) {
    if current_runtime_schema_state()
        .namespaces
        .contains_key(&namespace)
    {
        return;
    }

    let version = bump_pipeline_schema_version();
    apply_runtime_source_schema_state(RuntimeSchemaState {
        version,
        namespaces: BTreeMap::from([(namespace, output_metadata_for_arrow_schema(schema))]),
    });
}

async fn ingest_runtime_batches_into_core(
    request_id: u64,
    batches: Vec<crate::runtime_plugins::protocol::RuntimeIngestPartitionBatch>,
    offsets: Arc<Offsets>,
    _shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
) -> io::Result<()> {
    if batches.is_empty() {
        return Ok(());
    }

    let mut buffer_batches = Vec::with_capacity(batches.len());
    let mut derived_namespaces = BTreeMap::new();
    let mut total_rows = 0u64;
    let mut total_bytes = 0u64;
    for batch in batches {
        total_bytes = total_bytes.saturating_add(batch.arrow_stream_bytes.len() as u64);
        let mut stream = decode_record_batch_stream(batch.arrow_stream_bytes)?;
        let mut record_batches = Vec::new();
        while let Some(next_batch) = stream.next().await {
            let record_batch = next_batch.map_err(|err| io::Error::other(err.to_string()))?;
            total_rows = total_rows.saturating_add(record_batch.num_rows() as u64);
            record_batches.push(record_batch);
        }
        let schema = record_batches
            .first()
            .map(|batch| batch.schema())
            .unwrap_or_else(|| Arc::new(arrow::datatypes::Schema::empty()));
        let namespace = storage_namespace(&batch.namespace);
        derived_namespaces.insert(namespace.clone(), output_metadata_for_arrow_schema(&schema));
        let offsets_map = batch
            .offsets
            .into_iter()
            .map(|offset| (offset.key, offset.position))
            .collect();
        buffer_batches.push(IngestBufferBatch {
            offsets: offsets_map,
            sink_ref: batch.sink_ref,
            _namespace: namespace,
            _partition: storage_partition(&batch.partition),
            _time: batch.time,
            _schema_fingerprint: batch.schema_fingerprint,
            schema,
            record_batches: Some(record_batches),
            cdc_rows: batch.cdc_rows,
            checkpoint_update: batch
                .checkpoint_update
                .map(|update| (update.key, update.envelope)),
        });
    }
    if !derived_namespaces.is_empty() {
        let version = bump_pipeline_schema_version();
        let changed_namespaces = apply_runtime_source_schema_state(RuntimeSchemaState {
            version,
            namespaces: derived_namespaces,
        });
        for namespace in changed_namespaces {
            Config::sync_output_schema_namespace_blocking(&namespace)
                .await
                .map_err(io::Error::other)?;
        }
    }

    let arrow_bytes = buffer_batches
        .iter()
        .flat_map(|batch| batch.record_batches.as_ref().into_iter().flatten())
        .map(|batch| batch.get_array_memory_size())
        .sum::<usize>();
    crate::metrics::counters::add_messages(total_rows);
    crate::metrics::counters::add_source_bytes(total_bytes);
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    crate::buffer::wal_writer::submit(crate::buffer::wal_writer::WalCommitUnit {
        submit_id: request_id,
        batches: buffer_batches,
        offsets_db: offsets,
        raw_bytes: total_bytes as usize,
        arrow_bytes,
        done: done_tx,
    })
    .await
    .map_err(io::Error::other)?;
    done_rx
        .await
        .map_err(|err| io::Error::other(format!("WAL writer response dropped: {err}")))?
        .map_err(io::Error::other)
}

fn decode_plugin_data_frame(payload: Vec<u8>) -> io::Result<PluginDataFrame> {
    bincode::deserialize(&payload).map_err(|err| io::Error::other(err.to_string()))
}

fn ingest_source_payload_batches_into_core(
    request_id: u64,
    tasks: Vec<Vec<crate::runtime_plugins::protocol::RuntimeRawIngestBatch>>,
    offsets: Arc<Offsets>,
    shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
    ingest: Arc<Ingest>,
) -> io::Result<()> {
    if tasks.is_empty() {
        return Ok(());
    }

    let mut ingest_tasks = IngestTasks::new();
    for task in tasks {
        if task.is_empty() {
            continue;
        }
        let batches = task.into_iter().map(IngestBatch::from).collect::<Vec<_>>();
        ingest_tasks.add(
            IngestTask::new(batches, offsets.clone(), shared_output.clone())
                .with_submit_id(request_id),
        );
    }
    let ingest_tasks = Arc::new(ingest_tasks);
    let _ = ingest.ingest_file(&ingest_tasks, &offsets, shared_output);
    ingest.log_wal_ingest_pressure_snapshot();
    if crate::data_dir_capacity_exceeded() {
        return Err(io::Error::other(
            crate::take_data_dir_capacity_error().unwrap_or_else(|| {
                "DATA_DIR capacity exhausted and no reclaimable WAL remains to compact".to_string()
            }),
        ));
    }
    Ok(())
}

fn runtime_source_blocking_spawn_cap() -> usize {
    Config::getenv("INGEST_THREADS", "")
        .parse::<usize>()
        .ok()
        .filter(|value| *value > 0)
        .or_else(|| std::thread::available_parallelism().ok().map(|n| n.get()))
        .unwrap_or(4)
}

/// Join any finished blocking tasks, then wait if too many are still in-flight.
async fn runtime_source_throttle_blocking_tasks(
    pending_tasks: &mut JoinSet<io::Result<()>>,
) -> io::Result<()> {
    while let Some(joined) = pending_tasks.try_join_next() {
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
    let cap = runtime_source_blocking_spawn_cap();
    while pending_tasks.len() >= cap {
        match pending_tasks.join_next().await {
            Some(Ok(Ok(()))) => {}
            Some(Ok(Err(err))) => return Err(err),
            Some(Err(err)) => {
                return Err(io::Error::other(format!(
                    "runtime source task failed: {err}"
                )))
            }
            None => return Ok(()),
        }
    }
    Ok(())
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

async fn drain_runtime_source_ingest(
    pending_tasks: &mut JoinSet<io::Result<()>>,
    ingest: Arc<Ingest>,
) -> io::Result<()> {
    drain_runtime_source_tasks(pending_tasks).await?;
    tokio::task::spawn_blocking(move || ingest.wait_for_completion())
        .await
        .map_err(|err| io::Error::other(format!("runtime source ingest drain failed: {err}")))?;
    Ok(())
}

async fn runtime_discovery_completed_on_control_eof(
    connection: &mut RuntimeChildConnection,
    execution_mode: RuntimeExecutionMode,
) -> io::Result<bool> {
    if execution_mode != RuntimeExecutionMode::Discover {
        return Ok(false);
    }

    let status = match connection.try_exit_status()? {
        Some(status) => status,
        None => match timeout(Duration::from_secs(2), connection.child.wait()).await {
            Ok(status) => {
                let status = status?;
                unregister_runtime_plugin_child(connection.child.id());
                status
            }
            Err(_) => return Ok(false),
        },
    };
    if status.success() {
        // Some append-source runtimes perform discovery through the legacy core path
        // and exit cleanly after writing metadata instead of emitting Completed.
        return Ok(true);
    }
    Err(io::Error::other(format!(
        "runtime source exited during discovery before sending completion: {status}"
    )))
}

async fn stop_runtime_discovery_source(
    connection: &mut RuntimeChildConnection,
    pending_tasks: &mut JoinSet<io::Result<()>>,
) -> io::Result<()> {
    drain_runtime_source_tasks(pending_tasks).await?;
    let child_pid = connection.child.id();
    if !connection.has_exited()? {
        let _ = connection.child.kill().await;
        unregister_runtime_plugin_child(child_pid);
    }
    Ok(())
}

pub async fn sync_runtime_input_plugin(
    resolved: ResolvedRuntimePlugin,
    pipeline_name: String,
    execution_mode: RuntimeExecutionMode,
    source_once: bool,
    offsets: Arc<Offsets>,
    shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
) -> io::Result<()> {
    let session_token = Uuid::new_v4().to_string();
    let _offset_service = OffsetServiceEndpoint::spawn(offsets.clone(), session_token.clone())
        .map_err(|err| {
            io::Error::other(format!("failed to start runtime offset service: {err}"))
        })?;
    let mut connection = RuntimeChildConnection::spawn(
        resolved,
        pipeline_name,
        Some(_offset_service.env_value()),
        Some(session_token),
        1,
    )
    .await?;
    let start_request = build_source_start_request_for_pipeline(
        &connection.pipeline_name,
        execution_mode,
        source_once,
    )?;
    connection
        .send(&HostFrame::RunSource(start_request))
        .await?;

    let mut control_completed = false;
    let mut data_completed = false;
    let mut saw_unflushed_batches = false;
    let mut pending_source_tasks: JoinSet<io::Result<()>> = JoinSet::new();
    let (ingest_ack_tx, mut ingest_ack_rx) =
        tokio::sync::mpsc::unbounded_channel::<RuntimeIngestAck>();
    let ingest = Arc::new(Ingest::new_for_execution(execution_mode));
    let mut control_reader = BufferedRuntimeFrameReader::new();
    let mut data_reader = BufferedRuntimeFrameReader::new();
    loop {
        if execution_mode == RuntimeExecutionMode::Discover && Ingest::discovery_complete() {
            stop_runtime_discovery_source(&mut connection, &mut pending_source_tasks).await?;
            return Ok(());
        }

        while !control_completed {
            if let Some(control_frame) = control_reader.take_frame::<PluginFrame>()? {
                match control_frame {
                    PluginFrame::SourceEvent(SourceEvent::ContractsUpdate(contracts)) => {
                        match crate::plugins::source_contract::apply_runtime_source_namespace_contracts(
                            contracts,
                        )
                        .await
                        {
                            Ok(changed) => {
                                if changed {
                                    info!("Runtime source namespace contracts updated");
                                }
                            }
                            Err(err) => {
                                pending_source_tasks.abort_all();
                                return Err(io::Error::other(format!(
                                    "invalid runtime source namespace contracts: {err}"
                                )));
                            }
                        }
                    }
                    PluginFrame::SourceEvent(SourceEvent::SchemaStateUpdate(schema_state)) => {
                        let namespace_count = schema_state.namespaces.len();
                        let changed_namespaces = apply_runtime_source_schema_state(schema_state);
                        if namespace_count > 0 {
                            info!(
                                "Runtime source schema state update: {} namespaces received, {} changed",
                                namespace_count,
                                changed_namespaces.len()
                            );
                        }
                        if Config::truth_value(&Config::getenv(
                            "SCHEMA_SYNC_RUNTIME_SOURCE_STATE",
                            "false",
                        )) {
                            for namespace in changed_namespaces {
                                Config::sync_output_schema_namespace(&namespace);
                            }
                        } else if !changed_namespaces.is_empty() {
                            crate::metrics::counters::add_runtime_schema_state_publication_skipped(
                                1,
                            );
                            debug!(
                                "Runtime source schema state publication skipped for {} namespaces",
                                changed_namespaces.len()
                            );
                        }
                    }
                    PluginFrame::SourceEvent(SourceEvent::SchemaDelta(delta)) => {
                        let namespace_count = delta.namespaces.len();
                        let changed_namespaces =
                            apply_runtime_source_schema_state(RuntimeSchemaState {
                                version: delta.version,
                                namespaces: delta
                                    .namespaces
                                    .into_iter()
                                    .map(|(namespace, entry)| (namespace, entry.metadata))
                                    .collect(),
                            });
                        if namespace_count > 0 {
                            info!(
                                "Runtime source schema delta: {} namespaces received, {} changed",
                                namespace_count,
                                changed_namespaces.len()
                            );
                        }
                    }
                    PluginFrame::SourceEvent(SourceEvent::Completed) => {
                        control_completed = true;
                        drain_runtime_source_ingest(&mut pending_source_tasks, ingest.clone())
                            .await?;
                    }
                    PluginFrame::OffsetRequest(_) => {
                        pending_source_tasks.abort_all();
                        return Err(io::Error::other(
                            "runtime source offset requests must use the dedicated offset service channel",
                        ));
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
                if runtime_discovery_completed_on_control_eof(&mut connection, execution_mode)
                    .await?
                {
                    control_completed = true;
                    drain_runtime_source_ingest(&mut pending_source_tasks, ingest.clone()).await?;
                    continue;
                }
                pending_source_tasks.abort_all();
                return Err(io::Error::other(
                    "runtime source closed control channel before sending completion",
                ));
            }
            break;
        }

        while let Ok(ack) = ingest_ack_rx.try_recv() {
            connection.send(&HostFrame::IngestAck(ack)).await?;
        }

        if let Some(payload) = data_reader.take_frame_payload()? {
            let data_frame = tokio::task::spawn_blocking(move || decode_plugin_data_frame(payload))
                .await
                .map_err(|err| {
                    io::Error::other(format!("runtime data frame decode failed: {err}"))
                })??;
            match data_frame {
                PluginDataFrame::SourcePayloadBatches { request_id, tasks } => {
                    let non_empty_tasks = tasks.iter().filter(|task| !task.is_empty()).count();
                    if non_empty_tasks == 0 {
                        let _ = ingest_ack_tx.send(RuntimeIngestAck {
                            request_id,
                            error: None,
                        });
                    } else {
                        let ack_rx = crate::buffer::wal_writer::register_request_ack(
                            request_id,
                            non_empty_tasks,
                        );
                        let ack_tx = ingest_ack_tx.clone();
                        tokio::spawn(async move {
                            let error = match ack_rx.await {
                                Ok(Ok(())) => None,
                                Ok(Err(err)) => Some(err.to_string()),
                                Err(err) => {
                                    Some(format!("runtime ingest ack channel dropped: {err}"))
                                }
                            };
                            let _ = ack_tx.send(RuntimeIngestAck { request_id, error });
                        });
                        let raw_frame_bytes: usize =
                            tasks.iter().flat_map(|t| t.iter()).map(|b| b.bytes).sum();
                        if Config::debug_enabled() || Config::log_wal_enabled() {
                            let total_batches: usize = tasks.iter().map(Vec::len).sum();
                            info!(
                                "runtime source host: received {} source payload tasks ({} batches, {} bytes)",
                                tasks.len(),
                                total_batches,
                                raw_frame_bytes
                            );
                        }
                        if Config::log_wal_enabled() {
                            info!(
                                "runtime source host WAL: payload_frame_bytes={} pending_blocking_tasks={}/{}",
                                raw_frame_bytes,
                                pending_source_tasks.len(),
                                runtime_source_blocking_spawn_cap()
                            );
                        }
                        if let Err(err) =
                            runtime_source_throttle_blocking_tasks(&mut pending_source_tasks).await
                        {
                            crate::buffer::wal_writer::fail_request(request_id, err.to_string());
                            return Err(err);
                        }
                        let offsets = offsets.clone();
                        let shared_output = shared_output.clone();
                        let ingest = ingest.clone();
                        pending_source_tasks.spawn_blocking(move || {
                            let result =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    ingest_source_payload_batches_into_core(
                                        request_id,
                                        tasks,
                                        offsets,
                                        shared_output,
                                        ingest,
                                    )
                                }));
                            match result {
                                Ok(Ok(())) => Ok(()),
                                Ok(Err(err)) => {
                                    crate::buffer::wal_writer::fail_request(
                                        request_id,
                                        err.to_string(),
                                    );
                                    Err(err)
                                }
                                Err(_) => {
                                    let message = "runtime source ingest task panicked".to_string();
                                    crate::buffer::wal_writer::fail_request(
                                        request_id,
                                        message.clone(),
                                    );
                                    Err(io::Error::other(message))
                                }
                            }
                        });
                        saw_unflushed_batches = true;
                    }
                }
                PluginDataFrame::IngestBatches {
                    request_id,
                    batches,
                } => {
                    if batches.is_empty() {
                        let _ = ingest_ack_tx.send(RuntimeIngestAck {
                            request_id,
                            error: None,
                        });
                    } else {
                        let ack_rx = crate::buffer::wal_writer::register_request_ack(request_id, 1);
                        let ack_tx = ingest_ack_tx.clone();
                        tokio::spawn(async move {
                            let error = match ack_rx.await {
                                Ok(Ok(())) => None,
                                Ok(Err(err)) => Some(err.to_string()),
                                Err(err) => {
                                    Some(format!("runtime ingest ack channel dropped: {err}"))
                                }
                            };
                            let _ = ack_tx.send(RuntimeIngestAck { request_id, error });
                        });
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
                        if let Err(err) =
                            runtime_source_throttle_blocking_tasks(&mut pending_source_tasks).await
                        {
                            crate::buffer::wal_writer::fail_request(request_id, err.to_string());
                            return Err(err);
                        }
                        let offsets = offsets.clone();
                        let shared_output = shared_output.clone();
                        pending_source_tasks.spawn_blocking(move || {
                            let result =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    INGEST_RT
                                        .handle()
                                        .block_on(ingest_runtime_batches_into_core(
                                            request_id,
                                            batches,
                                            offsets,
                                            shared_output,
                                        ))
                                }));
                            match result {
                                Ok(Ok(())) => Ok(()),
                                Ok(Err(err)) => {
                                    crate::buffer::wal_writer::fail_request(
                                        request_id,
                                        err.to_string(),
                                    );
                                    Err(err)
                                }
                                Err(_) => {
                                    let message =
                                        "runtime prepared ingest task panicked".to_string();
                                    crate::buffer::wal_writer::fail_request(
                                        request_id,
                                        message.clone(),
                                    );
                                    Err(io::Error::other(message))
                                }
                            }
                        });
                        saw_unflushed_batches = true;
                    }
                }
                PluginDataFrame::SinkWrite(write) => {
                    let shared_output = shared_output.clone();
                    pending_source_tasks.spawn(async move {
                        let source_bytes = write.arrow_stream_bytes.len() as u64;
                        let decoded =
                            decode_record_batch_stream_with_stats(write.arrow_stream_bytes)?;
                        let stream = decoded.stream;
                        crate::metrics::counters::add_messages(decoded.rows);
                        crate::metrics::counters::add_source_bytes(source_bytes);
                        let namespace =
                            query_value_from_runtime_filename(&write.filename, "namespace");
                        if let Some(ref ns) = namespace {
                            apply_derived_runtime_schema(ns.clone(), &stream.schema());
                        }
                        let source_contract = write.source_contract.or_else(|| {
                            namespace.as_deref().and_then(
                                crate::plugins::source_contract::namespace_source_contract,
                            )
                        });
                        let ctx = crate::plugins::SinkWriteContext {
                            filename: write.filename,
                            compaction_id: write.compaction_id,
                            idempotency_key: write.idempotency_key,
                            wal_refs: write.wal_refs,
                            write_semantics: write.write_semantics,
                            schema_fingerprint: write.schema_fingerprint,
                            cdc_ctx: write.cdc_ctx.as_ref(),
                            source_contract: source_contract.as_ref(),
                        };
                        shared_output.sync_with_context(stream, ctx).await?;
                        Ok(())
                    });
                }
                PluginDataFrame::CheckpointUpdate { update } => {
                    drain_runtime_source_ingest(&mut pending_source_tasks, ingest.clone()).await?;
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
                    drain_runtime_source_ingest(&mut pending_source_tasks, ingest.clone()).await?;
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
            drain_runtime_source_ingest(&mut pending_source_tasks, ingest.clone()).await?;
            if saw_unflushed_batches {
                flush_all_segments(offsets.clone())
                    .await
                    .map_err(|err| io::Error::other(err.to_string()))?;
            }
            break;
        }

        tokio::select! {
            biased;
            maybe_ack = ingest_ack_rx.recv() => {
                if let Some(ack) = maybe_ack {
                    connection
                        .send(&HostFrame::IngestAck(ack))
                        .await?;
                    continue;
                }
            }
            control_ready = connection.control.readable(), if !control_completed => {
                match control_ready {
                    Ok(()) => control_reader.fill_from_ready(&connection.control).map_err(|err| {
                        io::Error::other(format!("runtime source control channel read failed: {err}"))
                    })?,
                    Err(err) if is_runtime_channel_eof(&err) => {
                        if runtime_discovery_completed_on_control_eof(
                            &mut connection,
                            execution_mode,
                        )
                        .await?
                        {
                            control_completed = true;
                            drain_runtime_source_ingest(&mut pending_source_tasks, ingest.clone())
                                .await?;
                            continue;
                        }
                        pending_source_tasks.abort_all();
                        return Err(io::Error::other(
                            "runtime source closed control channel before sending completion",
                        ));
                    }
                    Err(err) => {
                        pending_source_tasks.abort_all();
                        return Err(io::Error::other(format!(
                            "runtime source control channel readiness failed: {err}"
                        )));
                    }
                }
            }
            data_ready = connection.data.readable(), if !data_completed => {
                match data_ready {
                    Ok(()) => data_reader.fill_from_ready(&connection.data).map_err(|err| {
                        io::Error::other(format!("runtime source data channel read failed: {err}"))
                    })?,
                    Err(err) if is_runtime_channel_eof(&err) => {
                        data_completed = true;
                    }
                    Err(err) => {
                        pending_source_tasks.abort_all();
                        return Err(io::Error::other(format!(
                            "runtime source data channel readiness failed: {err}"
                        )));
                    }
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(100)), if execution_mode == RuntimeExecutionMode::Discover => {}
        }
    }

    drain_runtime_source_ingest(&mut pending_source_tasks, ingest).await?;
    Ok(())
}

fn runtime_execution_context(
    pipeline_name: &str,
    execution_mode: RuntimeExecutionMode,
) -> RuntimeExecutionContext {
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
    let time_partition_prefix = Config::get_time_partition_prefix();

    RuntimeExecutionContext {
        pipeline_name: pipeline_name.to_string(),
        workspace_name: Config::get_workspace_name(),
        data_dir: Config::get_data_dir(),
        execution_mode,
        output_layout: RuntimeOutputLayout {
            partition_fields,
            order_fields,
            time_partition_granularity,
            time_partition_prefix,
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
    if matches!(
        err.kind(),
        ErrorKind::BrokenPipe
            | ErrorKind::ConnectionAborted
            | ErrorKind::ConnectionReset
            | ErrorKind::NotConnected
            | ErrorKind::UnexpectedEof
    ) {
        return true;
    }

    let err = err.to_string();
    err.contains("unexpected runtime")
        || err.contains("unexpected sink")
        || err.contains("unexpected schema")
}

fn runtime_timeout(name: &str, default_secs: u64) -> Duration {
    Duration::from_secs(
        Config::getenv(name, &default_secs.to_string())
            .parse::<u64>()
            .ok()
            .filter(|value| *value > 0)
            .unwrap_or(default_secs),
    )
}

fn runtime_sink_request_timeout() -> Duration {
    runtime_timeout("RUNTIME_SINK_REQUEST_TIMEOUT_SECS", 600)
}

fn runtime_sink_schema_timeout() -> Duration {
    runtime_timeout("RUNTIME_SINK_SCHEMA_TIMEOUT_SECS", 120)
}

fn runtime_sink_payload_chunk_bytes() -> usize {
    Config::getenv(
        "RUNTIME_SINK_PAYLOAD_CHUNK_BYTES",
        &(64 * 1024 * 1024).to_string(),
    )
    .parse::<usize>()
    .ok()
    .filter(|value| *value > 0)
    .unwrap_or(64 * 1024 * 1024)
    .min(MAX_RUNTIME_SINK_CHUNK_BYTES)
    .min(MAX_RUNTIME_FRAME_BYTES.saturating_sub(64 * 1024))
}

fn runtime_sink_global_session_budget() -> usize {
    Config::getenv("RUNTIME_SINK_SESSION_BUDGET", "256")
        .parse::<usize>()
        .ok()
        .filter(|value| *value > 0)
        .unwrap_or(256)
        .clamp(1, 256)
}

fn runtime_sink_session_target() -> usize {
    // Compatibility mirror of the authoritative FlushBudgetSnapshot.
    crate::ingest::tuner::apply_env_caps();
    crate::metrics::counters::RUNTIME_SINK_POOL_TARGET
        .load(Ordering::Relaxed)
        .clamp(1, 256)
        .min(runtime_sink_global_session_budget())
}

fn runtime_sink_process_cap() -> usize {
    Config::getenv("RUNTIME_SINK_CONNECTION_POOL_SIZE", "16")
        .parse::<usize>()
        .ok()
        .filter(|value| *value > 0)
        .unwrap_or(16)
        .min(16)
}

fn runtime_sink_session_capacity(adapter_limit: usize) -> usize {
    let requested = Config::getenv("RUNTIME_SINK_SESSIONS_PER_CHILD", "2")
        .parse::<usize>()
        .ok()
        .filter(|value| *value > 0)
        .unwrap_or(2)
        .min(16);
    requested
        .min(adapter_limit.max(1))
        .min(runtime_sink_global_session_budget())
        .max(1)
}

fn div_ceil_usize(value: usize, divisor: usize) -> usize {
    value.saturating_add(divisor.saturating_sub(1)) / divisor.max(1)
}

#[derive(Debug)]
struct RuntimeSinkBudgetBinding {
    binding: RuntimeBinding,
    workers: usize,
    session_capacity: usize,
}

#[derive(Debug)]
struct RuntimeSinkProcessBudgetState {
    minimum_bindings: usize,
    next_binding_id: usize,
    bindings: BTreeMap<usize, RuntimeSinkBudgetBinding>,
    last_clamp_log: Option<(usize, usize)>,
}

/// One child-process budget shared by every runtime data sink in a pipeline.
///
/// The tuner target is total sink-session demand. Each binding receives a fair
/// share, then converts that share to child processes using its immutable
/// adapter/session capacity. The legacy connection-pool setting is only a hard
/// process cap; every live binding still retains one child.
pub(crate) struct RuntimeSinkProcessBudget {
    pipeline_name: String,
    state: std::sync::Mutex<RuntimeSinkProcessBudgetState>,
    #[cfg(test)]
    fixed_session_target: Option<usize>,
    #[cfg(test)]
    fixed_process_cap: Option<usize>,
}

static RUNTIME_SINK_PROCESS_BUDGETS: Lazy<
    std::sync::Mutex<HashMap<String, Weak<RuntimeSinkProcessBudget>>>,
> = Lazy::new(|| std::sync::Mutex::new(HashMap::new()));

impl RuntimeSinkProcessBudget {
    pub(crate) fn for_pipeline(
        pipeline_name: &str,
        minimum_bindings: usize,
    ) -> Arc<RuntimeSinkProcessBudget> {
        let mut budgets = RUNTIME_SINK_PROCESS_BUDGETS
            .lock()
            .expect("runtime sink process budget registry poisoned");
        budgets.retain(|_, budget| budget.strong_count() > 0);
        let budget = budgets
            .get(pipeline_name)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| {
                let budget = Arc::new(Self {
                    pipeline_name: pipeline_name.to_string(),
                    state: std::sync::Mutex::new(RuntimeSinkProcessBudgetState {
                        minimum_bindings: minimum_bindings.max(1),
                        next_binding_id: 0,
                        bindings: BTreeMap::new(),
                        last_clamp_log: None,
                    }),
                    #[cfg(test)]
                    fixed_session_target: None,
                    #[cfg(test)]
                    fixed_process_cap: None,
                });
                budgets.insert(pipeline_name.to_string(), Arc::downgrade(&budget));
                budget
            });
        budget.ensure_minimum_bindings(minimum_bindings);
        budget
    }

    fn session_target(&self) -> usize {
        #[cfg(test)]
        if let Some(target) = self.fixed_session_target {
            return target.max(1);
        }
        runtime_sink_session_target()
    }

    fn process_cap(&self) -> usize {
        #[cfg(test)]
        if let Some(cap) = self.fixed_process_cap {
            return cap.max(1);
        }
        runtime_sink_process_cap()
    }

    fn ensure_minimum_bindings(&self, minimum_bindings: usize) {
        let process_cap = self.process_cap();
        let mut state = self
            .state
            .lock()
            .expect("runtime sink process budget poisoned");
        state.minimum_bindings = state.minimum_bindings.max(minimum_bindings.max(1));
        Self::log_clamp_if_needed(&self.pipeline_name, process_cap, &mut state);
    }

    fn log_clamp_if_needed(
        pipeline_name: &str,
        process_cap: usize,
        state: &mut RuntimeSinkProcessBudgetState,
    ) {
        let required = state.minimum_bindings.max(state.bindings.len()).max(1);
        if process_cap < required && state.last_clamp_log != Some((process_cap, required)) {
            warn!(
                "runtime sink hard process cap {} is below {} configured bindings for pipeline '{}'; retaining one worker per binding",
                process_cap, required, pipeline_name
            );
            state.last_clamp_log = Some((process_cap, required));
        }
    }

    fn worker_allowances(
        state: &RuntimeSinkProcessBudgetState,
        session_target: usize,
        process_cap: usize,
    ) -> BTreeMap<usize, usize> {
        let slot_count = state.minimum_bindings.max(state.bindings.len()).max(1);
        let effective_sessions = session_target.max(slot_count);
        let effective_process_cap = process_cap.max(slot_count);
        let mut desired = vec![1usize; slot_count];
        for (position, (_, binding)) in state.bindings.iter().enumerate() {
            let fair_sessions = effective_sessions / slot_count
                + usize::from(position < effective_sessions % slot_count);
            desired[position] = div_ceil_usize(fair_sessions, binding.session_capacity).max(1);
        }
        let mut allowance = vec![1usize; slot_count];
        let mut allocated = slot_count;
        while allocated < effective_process_cap {
            let mut progressed = false;
            for position in 0..slot_count {
                if allocated >= effective_process_cap {
                    break;
                }
                if allowance[position] < desired[position] {
                    allowance[position] += 1;
                    allocated += 1;
                    progressed = true;
                }
            }
            if !progressed {
                break;
            }
        }
        state
            .bindings
            .keys()
            .enumerate()
            .map(|(position, id)| (*id, allowance[position]))
            .collect()
    }

    fn register(
        self: &Arc<Self>,
        binding: RuntimeBinding,
        session_capacity: usize,
    ) -> RuntimeSinkBudgetRegistration {
        let process_cap = self.process_cap();
        let binding_id = {
            let mut state = self
                .state
                .lock()
                .expect("runtime sink process budget poisoned");
            let binding_id = state.next_binding_id;
            state.next_binding_id = state.next_binding_id.saturating_add(1);
            state.bindings.insert(
                binding_id,
                RuntimeSinkBudgetBinding {
                    binding,
                    workers: 0,
                    session_capacity: session_capacity.max(1),
                },
            );
            Self::log_clamp_if_needed(&self.pipeline_name, process_cap, &mut state);
            binding_id
        };
        RuntimeSinkBudgetRegistration {
            inner: Arc::new(RuntimeSinkBudgetRegistrationInner {
                budget: Arc::clone(self),
                binding_id,
            }),
        }
    }

    fn try_reserve(&self, binding_id: usize, initial: bool) -> bool {
        let session_target = self.session_target();
        let process_cap = self.process_cap();
        let mut state = self
            .state
            .lock()
            .expect("runtime sink process budget poisoned");
        Self::log_clamp_if_needed(&self.pipeline_name, process_cap, &mut state);
        let allowances = Self::worker_allowances(&state, session_target, process_cap);
        let admitted_limit = allowances.values().sum::<usize>();
        let active_workers = state
            .bindings
            .values()
            .map(|binding| binding.workers)
            .sum::<usize>();
        if active_workers >= admitted_limit {
            return false;
        }
        let allowance = allowances.get(&binding_id).copied().unwrap_or(0);
        let Some(binding_state) = state.bindings.get_mut(&binding_id) else {
            return false;
        };
        if binding_state.workers >= allowance && !(initial && binding_state.workers == 0) {
            return false;
        }
        binding_state.workers = binding_state.workers.saturating_add(1);
        true
    }

    fn desired_workers(&self, binding_id: usize) -> usize {
        let state = self
            .state
            .lock()
            .expect("runtime sink process budget poisoned");
        Self::worker_allowances(&state, self.session_target(), self.process_cap())
            .get(&binding_id)
            .copied()
            .unwrap_or(1)
    }

    fn release(&self, binding_id: usize) {
        let mut state = self
            .state
            .lock()
            .expect("runtime sink process budget poisoned");
        if let Some(binding) = state.bindings.get_mut(&binding_id) {
            binding.workers = binding.workers.saturating_sub(1);
        }
    }

    fn unregister(&self, binding_id: usize) {
        let mut state = self
            .state
            .lock()
            .expect("runtime sink process budget poisoned");
        if let Some(binding) = state.bindings.remove(&binding_id) {
            debug_assert_eq!(
                binding.workers, 0,
                "runtime sink binding {:?} dropped with live budget permits",
                binding.binding
            );
        }
    }

    fn active_workers(&self) -> usize {
        self.state
            .lock()
            .expect("runtime sink process budget poisoned")
            .bindings
            .values()
            .map(|binding| binding.workers)
            .sum()
    }

    #[cfg(test)]
    fn new_for_test(
        session_target: usize,
        process_cap: usize,
        minimum_bindings: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            pipeline_name: "runtime-sink-budget-test".to_string(),
            state: std::sync::Mutex::new(RuntimeSinkProcessBudgetState {
                minimum_bindings: minimum_bindings.max(1),
                next_binding_id: 0,
                bindings: BTreeMap::new(),
                last_clamp_log: None,
            }),
            fixed_session_target: Some(session_target.max(1)),
            fixed_process_cap: Some(process_cap.max(1)),
        })
    }
}

#[cfg(test)]
fn reset_runtime_sink_process_budgets_for_test() {
    RUNTIME_SINK_PROCESS_BUDGETS
        .lock()
        .expect("runtime sink process budget registry poisoned")
        .clear();
}

#[derive(Clone)]
struct RuntimeSinkBudgetRegistration {
    inner: Arc<RuntimeSinkBudgetRegistrationInner>,
}

struct RuntimeSinkBudgetRegistrationInner {
    budget: Arc<RuntimeSinkProcessBudget>,
    binding_id: usize,
}

impl RuntimeSinkBudgetRegistration {
    fn acquire_initial(&self) -> io::Result<RuntimeSinkProcessPermit> {
        if self.inner.budget.try_reserve(self.inner.binding_id, true) {
            Ok(RuntimeSinkProcessPermit {
                registration: self.clone(),
            })
        } else {
            Err(io::Error::other(format!(
                "runtime sink global process budget has no initial permit for {:?}",
                self.binding()
            )))
        }
    }

    fn try_acquire_additional(&self) -> Option<RuntimeSinkProcessPermit> {
        self.inner
            .budget
            .try_reserve(self.inner.binding_id, false)
            .then(|| RuntimeSinkProcessPermit {
                registration: self.clone(),
            })
    }

    fn desired_workers(&self) -> usize {
        self.inner.budget.desired_workers(self.inner.binding_id)
    }

    fn binding(&self) -> RuntimeBinding {
        self.inner
            .budget
            .state
            .lock()
            .expect("runtime sink process budget poisoned")
            .bindings
            .get(&self.inner.binding_id)
            .map(|binding| binding.binding)
            .unwrap_or(RuntimeBinding::Primary)
    }

    #[cfg(test)]
    fn active_workers(&self) -> usize {
        self.inner
            .budget
            .state
            .lock()
            .expect("runtime sink process budget poisoned")
            .bindings
            .get(&self.inner.binding_id)
            .map(|binding| binding.workers)
            .unwrap_or(0)
    }
}

impl Drop for RuntimeSinkBudgetRegistrationInner {
    fn drop(&mut self) {
        self.budget.unregister(self.binding_id);
    }
}

struct RuntimeSinkProcessPermit {
    registration: RuntimeSinkBudgetRegistration,
}

impl Drop for RuntimeSinkProcessPermit {
    fn drop(&mut self) {
        self.registration
            .inner
            .budget
            .release(self.registration.inner.binding_id);
    }
}

#[derive(Clone, Debug)]
struct RuntimeSinkConnectionFailure {
    kind: ErrorKind,
    message: String,
}

impl RuntimeSinkConnectionFailure {
    fn into_io(self) -> io::Error {
        io::Error::new(self.kind, self.message)
    }
}

struct RuntimeSinkConnectionShared {
    child_pid: Option<u32>,
    alive: AtomicBool,
    failure: std::sync::Mutex<Option<RuntimeSinkConnectionFailure>>,
    pending: std::sync::Mutex<HashMap<u64, mpsc::Sender<PluginFrame>>>,
    admin_waiter: std::sync::Mutex<
        Option<oneshot::Sender<Result<PluginFrame, RuntimeSinkConnectionFailure>>>,
    >,
}

impl RuntimeSinkConnectionShared {
    fn failure(&self) -> Option<RuntimeSinkConnectionFailure> {
        self.failure
            .lock()
            .expect("runtime sink connection failure poisoned")
            .clone()
    }

    fn fail(&self, kind: ErrorKind, message: impl Into<String>) {
        if !self.alive.swap(false, Ordering::AcqRel) {
            return;
        }
        let failure = RuntimeSinkConnectionFailure {
            kind,
            message: message.into(),
        };
        *self
            .failure
            .lock()
            .expect("runtime sink connection failure poisoned") = Some(failure.clone());
        self.pending
            .lock()
            .expect("runtime sink pending sessions poisoned")
            .clear();
        if let Some(waiter) = self
            .admin_waiter
            .lock()
            .expect("runtime sink admin waiter poisoned")
            .take()
        {
            let _ = waiter.send(Err(failure));
        }
        if let Some(child_pid) = self.child_pid {
            terminate_runtime_plugin_child_pid(child_pid);
            unregister_runtime_plugin_child(Some(child_pid));
        }
    }
}

struct RuntimeSinkMultiplexConnection {
    _child: Mutex<RuntimePluginChild>,
    control_writer: Mutex<OwnedWriteHalf>,
    data_writer: Mutex<OwnedWriteHalf>,
    shared: Arc<RuntimeSinkConnectionShared>,
    control_reader_task: tokio::task::JoinHandle<()>,
}

impl RuntimeSinkMultiplexConnection {
    fn start(connection: RuntimeChildConnection) -> Arc<Self> {
        let RuntimeChildConnection {
            child,
            control,
            data,
            ..
        } = connection;
        let child_pid = child.id();
        let (control_reader, control_writer) = control.into_split();
        let (_data_reader, data_writer) = data.into_split();
        let shared = Arc::new(RuntimeSinkConnectionShared {
            child_pid,
            alive: AtomicBool::new(true),
            failure: std::sync::Mutex::new(None),
            pending: std::sync::Mutex::new(HashMap::new()),
            admin_waiter: std::sync::Mutex::new(None),
        });
        let reader_shared = Arc::clone(&shared);
        let control_reader_task = tokio::spawn(async move {
            run_runtime_sink_control_demux(control_reader, reader_shared).await;
        });
        Arc::new(Self {
            _child: Mutex::new(child),
            control_writer: Mutex::new(control_writer),
            data_writer: Mutex::new(data_writer),
            shared,
            control_reader_task,
        })
    }

    fn is_alive(&self) -> bool {
        self.shared.alive.load(Ordering::Acquire)
    }

    fn failure(&self) -> io::Error {
        self.shared
            .failure()
            .unwrap_or(RuntimeSinkConnectionFailure {
                kind: ErrorKind::BrokenPipe,
                message: "runtime sink connection is closed".to_string(),
            })
            .into_io()
    }

    fn register_session(self: &Arc<Self>, request_id: u64) -> io::Result<RuntimeSinkSession> {
        if !self.is_alive() {
            return Err(self.failure());
        }
        let (sender, receiver) = mpsc::channel(2);
        let mut pending = self
            .shared
            .pending
            .lock()
            .expect("runtime sink pending sessions poisoned");
        if !self.is_alive() {
            return Err(self.failure());
        }
        if pending.contains_key(&request_id) {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("duplicate runtime sink request id {request_id}"),
            ));
        }
        pending.insert(request_id, sender);
        Ok(RuntimeSinkSession {
            request_id,
            receiver,
            connection: Arc::clone(self),
        })
    }

    async fn send_control(&self, frame: &HostFrame) -> io::Result<()> {
        if !self.is_alive() {
            return Err(self.failure());
        }
        let result = {
            let mut writer = self.control_writer.lock().await;
            write_frame(&mut *writer, frame).await
        };
        if let Err(err) = &result {
            self.shared.fail(
                err.kind(),
                format!("runtime sink control write failed: {err}"),
            );
        }
        result
    }

    async fn send_data(&self, frame: &HostDataFrame) -> io::Result<()> {
        if !self.is_alive() {
            return Err(self.failure());
        }
        let payload_bytes = match frame {
            HostDataFrame::SinkChunk(chunk) => chunk.arrow_stream_bytes.len() as u64,
            HostDataFrame::FinishSink(_) => 0,
        };
        let result = {
            let mut writer = self.data_writer.lock().await;
            write_frame(&mut *writer, frame).await
        };
        if let Err(err) = &result {
            self.shared
                .fail(err.kind(), format!("runtime sink data write failed: {err}"));
        } else {
            crate::metrics::counters::record_runtime_sink_ipc(payload_bytes, 1);
        }
        result
    }

    async fn send_admin(&self, frame: &HostFrame) -> io::Result<PluginFrame> {
        if !self.is_alive() {
            return Err(self.failure());
        }
        let (sender, receiver) = oneshot::channel();
        {
            let mut waiter = self
                .shared
                .admin_waiter
                .lock()
                .expect("runtime sink admin waiter poisoned");
            if waiter.is_some() {
                return Err(io::Error::other(
                    "concurrent runtime sink administrative request",
                ));
            }
            *waiter = Some(sender);
        }
        self.send_control(frame).await?;
        receiver
            .await
            .map_err(|_| self.failure())?
            .map_err(RuntimeSinkConnectionFailure::into_io)
    }
}

impl Drop for RuntimeSinkMultiplexConnection {
    fn drop(&mut self) {
        self.control_reader_task.abort();
        self.shared.fail(
            ErrorKind::BrokenPipe,
            "runtime sink connection dropped before session completion",
        );
    }
}

async fn run_runtime_sink_control_demux(
    mut reader: OwnedReadHalf,
    shared: Arc<RuntimeSinkConnectionShared>,
) {
    loop {
        let frame = match read_frame::<_, PluginFrame>(&mut reader).await {
            Ok(frame) => frame,
            Err(err) => {
                shared.fail(
                    err.kind(),
                    format!("runtime sink control connection failed: {err}"),
                );
                return;
            }
        };
        let request_id = match &frame {
            PluginFrame::PrepareAck(ack) => Some(ack.request_id),
            PluginFrame::SinkAck(ack) => Some(ack.request_id),
            PluginFrame::SinkError(err) => Some(err.request_id),
            PluginFrame::SchemaStateRefreshRequired(refresh) => Some(refresh.request_id),
            _ => None,
        };
        if let Some(request_id) = request_id {
            let sender = shared
                .pending
                .lock()
                .expect("runtime sink pending sessions poisoned")
                .get(&request_id)
                .cloned();
            let Some(sender) = sender else {
                shared.fail(
                    ErrorKind::InvalidData,
                    format!("runtime sink response for unknown request {request_id}"),
                );
                return;
            };
            if sender.try_send(frame).is_err() {
                shared.fail(
                    ErrorKind::InvalidData,
                    format!("runtime sink response queue overflow for request {request_id}"),
                );
                return;
            }
            continue;
        }
        match frame {
            PluginFrame::Installed => {
                let waiter = shared
                    .admin_waiter
                    .lock()
                    .expect("runtime sink admin waiter poisoned")
                    .take();
                let Some(waiter) = waiter else {
                    shared.fail(
                        ErrorKind::InvalidData,
                        "stale runtime sink install acknowledgement",
                    );
                    return;
                };
                let _ = waiter.send(Ok(PluginFrame::Installed));
            }
            PluginFrame::Error(message) => {
                shared.fail(ErrorKind::Other, format!("runtime sink error: {message}"));
                return;
            }
            other => {
                shared.fail(
                    ErrorKind::InvalidData,
                    format!("unexpected runtime sink control frame: {other:?}"),
                );
                return;
            }
        }
    }
}

struct RuntimeSinkSession {
    request_id: u64,
    receiver: mpsc::Receiver<PluginFrame>,
    connection: Arc<RuntimeSinkMultiplexConnection>,
}

impl RuntimeSinkSession {
    async fn recv(&mut self) -> io::Result<PluginFrame> {
        if !self.connection.is_alive() {
            return Err(self.connection.failure());
        }
        let frame = self
            .receiver
            .recv()
            .await
            .ok_or_else(|| self.connection.failure())?;
        if !self.connection.is_alive() {
            return Err(self.connection.failure());
        }
        Ok(frame)
    }
}

impl Drop for RuntimeSinkSession {
    fn drop(&mut self) {
        self.connection
            .shared
            .pending
            .lock()
            .expect("runtime sink pending sessions poisoned")
            .remove(&self.request_id);
    }
}

struct RuntimeSinkWorker {
    id: usize,
    connection: Mutex<Arc<RuntimeSinkMultiplexConnection>>,
    restart_lock: Mutex<()>,
    installed_schema_version: AtomicU64,
    has_installed_schema: AtomicBool,
    _process_permit: RuntimeSinkProcessPermit,
}

impl RuntimeSinkWorker {
    fn installed_schema_version(&self) -> Option<u64> {
        self.has_installed_schema
            .load(Ordering::Acquire)
            .then(|| self.installed_schema_version.load(Ordering::Acquire))
    }

    fn set_installed_schema_version(&self, version: u64) {
        self.installed_schema_version
            .store(version, Ordering::Release);
        self.has_installed_schema.store(true, Ordering::Release);
    }

    async fn connection(&self) -> Arc<RuntimeSinkMultiplexConnection> {
        self.connection.lock().await.clone()
    }
}

struct RuntimeSinkSlotState {
    available: VecDeque<Arc<RuntimeSinkWorker>>,
    free_by_worker: HashMap<usize, usize>,
}

struct RuntimeSinkSlotQueue {
    capacity_per_worker: usize,
    state: std::sync::Mutex<RuntimeSinkSlotState>,
    notify: Notify,
}

impl RuntimeSinkSlotQueue {
    fn new(capacity_per_worker: usize) -> Arc<Self> {
        Arc::new(Self {
            capacity_per_worker,
            state: std::sync::Mutex::new(RuntimeSinkSlotState {
                available: VecDeque::new(),
                free_by_worker: HashMap::new(),
            }),
            notify: Notify::new(),
        })
    }

    fn add_worker(&self, worker: Arc<RuntimeSinkWorker>) {
        let mut state = self.state.lock().expect("runtime sink slot queue poisoned");
        state
            .free_by_worker
            .insert(worker.id, self.capacity_per_worker);
        state.available.push_back(worker);
        self.notify.notify_waiters();
    }

    fn try_acquire(&self) -> Option<Arc<RuntimeSinkWorker>> {
        let mut state = self.state.lock().expect("runtime sink slot queue poisoned");
        let worker = state.available.pop_front()?;
        let free = state
            .free_by_worker
            .get_mut(&worker.id)
            .expect("runtime sink worker missing from slot queue");
        *free = free.saturating_sub(1);
        if *free > 0 {
            state.available.push_back(Arc::clone(&worker));
        }
        Some(worker)
    }

    fn has_available(&self) -> bool {
        !self
            .state
            .lock()
            .expect("runtime sink slot queue poisoned")
            .available
            .is_empty()
    }

    fn remove_idle_worker(&self, worker_id: usize) -> bool {
        let mut state = self.state.lock().expect("runtime sink slot queue poisoned");
        if state.free_by_worker.get(&worker_id).copied() != Some(self.capacity_per_worker) {
            return false;
        }
        state.free_by_worker.remove(&worker_id);
        state.available.retain(|worker| worker.id != worker_id);
        true
    }

    fn release(&self, worker: Arc<RuntimeSinkWorker>) {
        let mut state = self.state.lock().expect("runtime sink slot queue poisoned");
        let free = state
            .free_by_worker
            .get_mut(&worker.id)
            .expect("runtime sink worker missing from slot queue");
        let was_full = *free == 0;
        *free = (*free + 1).min(self.capacity_per_worker);
        if was_full {
            state.available.push_back(worker);
        }
        self.notify.notify_one();
    }
}

struct RuntimeSinkConnectionPool {
    install_request: RuntimeSinkInstallRequest,
    resolved: ResolvedRuntimePlugin,
    pipeline_name: String,
    budget_registration: RuntimeSinkBudgetRegistration,
    workers: tokio::sync::Mutex<Vec<Arc<RuntimeSinkWorker>>>,
    slots: Arc<RuntimeSinkSlotQueue>,
    next_worker_id: AtomicUsize,
    worker_count: AtomicUsize,
    schema_publish_lock: tokio::sync::Mutex<()>,
    schema_apply_fence: Arc<tokio::sync::RwLock<()>>,
    has_published_schema_state: AtomicBool,
    published_schema_version: std::sync::atomic::AtomicU64,
    maintenance_abort: std::sync::Mutex<Option<tokio::task::AbortHandle>>,
}

struct RuntimeSinkWorkerLease {
    slots: Arc<RuntimeSinkSlotQueue>,
    worker: Arc<RuntimeSinkWorker>,
}

impl Drop for RuntimeSinkWorkerLease {
    fn drop(&mut self) {
        crate::metrics::counters::dec_runtime_sink_active_sessions();
        self.slots.release(Arc::clone(&self.worker));
    }
}

struct RuntimeSinkPoolWaitMetrics {
    started: Instant,
}

impl RuntimeSinkPoolWaitMetrics {
    fn new() -> Self {
        crate::metrics::counters::inc_runtime_sink_pool_waiters();
        Self {
            started: Instant::now(),
        }
    }
}

impl Drop for RuntimeSinkPoolWaitMetrics {
    fn drop(&mut self) {
        crate::metrics::counters::dec_runtime_sink_pool_waiters();
        crate::metrics::counters::record_runtime_sink_pool_acquire_wait(self.started.elapsed());
    }
}

async fn spawn_installed_runtime_sink_connection(
    resolved: &ResolvedRuntimePlugin,
    pipeline_name: &str,
    install_request: &RuntimeSinkInstallRequest,
    session_capacity: usize,
) -> io::Result<(Arc<RuntimeSinkMultiplexConnection>, u64)> {
    let mut last_error = None;
    for attempt in 0..2 {
        let mut connection = RuntimeChildConnection::spawn(
            resolved.clone(),
            pipeline_name.to_string(),
            None,
            None,
            session_capacity,
        )
        .await?;
        let install_result = async {
            connection
                .send(&HostFrame::InstallSink(install_request.clone()))
                .await?;
            expect_install_ack(&mut connection, "sink").await?;
            let schema_state = current_runtime_schema_state();
            connection
                .send(&HostFrame::InstallSchemaState(
                    RuntimeSchemaStateInstallRequest {
                        schema_state: RuntimeSchemaState {
                            version: schema_state.version,
                            namespaces: schema_state.namespaces.clone(),
                        },
                    },
                ))
                .await?;
            expect_install_ack(&mut connection, "schema state").await?;
            connection.installed_schema_version = Some(schema_state.version);
            Ok::<u64, io::Error>(schema_state.version)
        }
        .await;
        match install_result {
            Ok(version) => {
                return Ok((RuntimeSinkMultiplexConnection::start(connection), version));
            }
            Err(err)
                if attempt == 0
                    && (connection.has_exited()? || should_retry_runtime_connection(&err)) =>
            {
                warn!(
                    "runtime sink startup install failed, restarting child: {}",
                    err
                );
                last_error = Some(err);
            }
            Err(err) => return Err(err),
        }
    }
    Err(last_error.unwrap_or_else(|| io::Error::other("runtime sink startup install failed")))
}

impl RuntimeSinkConnectionPool {
    async fn new(
        resolved: ResolvedRuntimePlugin,
        pipeline_name: String,
        install_request: RuntimeSinkInstallRequest,
        process_budget: Arc<RuntimeSinkProcessBudget>,
        adapter_session_limit: usize,
    ) -> io::Result<Arc<Self>> {
        let session_capacity = runtime_sink_session_capacity(adapter_session_limit);
        let budget_registration =
            process_budget.register(install_request.binding, session_capacity);
        let process_permit = budget_registration.acquire_initial()?;
        let (connection, schema_version) = spawn_installed_runtime_sink_connection(
            &resolved,
            &pipeline_name,
            &install_request,
            session_capacity,
        )
        .await?;
        let slots = RuntimeSinkSlotQueue::new(session_capacity);
        let first_worker = Arc::new(RuntimeSinkWorker {
            id: 0,
            connection: Mutex::new(connection),
            restart_lock: Mutex::new(()),
            installed_schema_version: AtomicU64::new(schema_version),
            has_installed_schema: AtomicBool::new(true),
            _process_permit: process_permit,
        });
        slots.add_worker(Arc::clone(&first_worker));
        let workers = vec![first_worker];
        let pool = Arc::new(Self {
            install_request,
            resolved,
            pipeline_name,
            workers: tokio::sync::Mutex::new(workers),
            budget_registration,
            slots,
            next_worker_id: AtomicUsize::new(1),
            worker_count: AtomicUsize::new(1),
            schema_publish_lock: tokio::sync::Mutex::new(()),
            schema_apply_fence: Arc::new(tokio::sync::RwLock::new(())),
            has_published_schema_state: AtomicBool::new(false),
            published_schema_version: std::sync::atomic::AtomicU64::new(0),
            maintenance_abort: std::sync::Mutex::new(None),
        });
        pool.start_idle_maintenance();
        Ok(pool)
    }

    fn start_idle_maintenance(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            interval.tick().await;
            loop {
                interval.tick().await;
                let Some(pool) = weak.upgrade() else {
                    return;
                };
                pool.maybe_shrink_idle().await;
            }
        });
        *self
            .maintenance_abort
            .lock()
            .expect("runtime sink maintenance task poisoned") = Some(task.abort_handle());
    }

    /// Spawn at most one child, and only after every existing child is full.
    async fn maybe_grow_one(&self) -> io::Result<bool> {
        let mut workers = self.workers.lock().await;
        if self.slots.has_available() {
            return Ok(false);
        }
        let Some(process_permit) = self.budget_registration.try_acquire_additional() else {
            return Ok(false);
        };
        let (connection, schema_version) = spawn_installed_runtime_sink_connection(
            &self.resolved,
            &self.pipeline_name,
            &self.install_request,
            self.slots.capacity_per_worker,
        )
        .await?;
        let worker = Arc::new(RuntimeSinkWorker {
            id: self.next_worker_id.fetch_add(1, Ordering::Relaxed),
            connection: Mutex::new(connection),
            restart_lock: Mutex::new(()),
            installed_schema_version: AtomicU64::new(schema_version),
            has_installed_schema: AtomicBool::new(true),
            _process_permit: process_permit,
        });
        self.slots.add_worker(Arc::clone(&worker));
        workers.push(worker);
        self.worker_count.store(workers.len(), Ordering::Release);
        info!(
            "tune: runtime_sink_pool grew binding={:?} workers={} global_active={} session_target={} per_child={}",
            self.budget_registration.binding(),
            workers.len(),
            self.budget_registration.inner.budget.active_workers(),
            self.budget_registration.inner.budget.session_target(),
            self.slots.capacity_per_worker,
        );
        Ok(true)
    }

    async fn maybe_shrink_idle(&self) {
        let desired = self.budget_registration.desired_workers().max(1);
        let mut removed = Vec::new();
        {
            let mut workers = self.workers.lock().await;
            while workers.len() > desired {
                let Some(position) = workers
                    .iter()
                    .rposition(|worker| self.slots.remove_idle_worker(worker.id))
                else {
                    break;
                };
                removed.push(workers.remove(position));
            }
            self.worker_count.store(workers.len(), Ordering::Release);
            if !removed.is_empty() {
                info!(
                    "tune: runtime_sink_pool shrank binding={:?} workers={} removed={} session_target={} per_child={}",
                    self.budget_registration.binding(),
                    workers.len(),
                    removed.len(),
                    self.budget_registration.inner.budget.session_target(),
                    self.slots.capacity_per_worker,
                );
            }
        }
        drop(removed);
    }

    async fn acquire(&self) -> io::Result<RuntimeSinkWorkerLease> {
        let _wait_metrics = RuntimeSinkPoolWaitMetrics::new();
        self.maybe_shrink_idle().await;
        let worker = loop {
            let notified = self.slots.notify.notified();
            if let Some(worker) = self.slots.try_acquire() {
                break worker;
            }
            if self.maybe_grow_one().await? {
                continue;
            }
            notified.await;
        };
        crate::metrics::counters::inc_runtime_sink_active_sessions();
        Ok(RuntimeSinkWorkerLease {
            slots: Arc::clone(&self.slots),
            worker,
        })
    }

    fn has_published_schema_version(&self, schema_version: u64) -> bool {
        self.has_published_schema_state.load(Ordering::Acquire)
            && self.published_schema_version.load(Ordering::Acquire) >= schema_version
    }

    fn mark_schema_version_published(&self, schema_version: u64) {
        self.published_schema_version
            .fetch_max(schema_version, Ordering::AcqRel);
        self.has_published_schema_state
            .store(true, Ordering::Release);
    }

    fn record_schema_publication_skipped(&self) {
        crate::metrics::counters::add_runtime_schema_state_publication_skipped(
            self.worker_count.load(Ordering::Acquire).max(1) as u64,
        );
    }

    async fn refresh_published_schema_version(&self) {
        let workers = {
            let workers = self.workers.lock().await;
            workers.iter().cloned().collect::<Vec<_>>()
        };
        let mut minimum_version: Option<u64> = None;
        for worker in workers {
            let Some(installed_version) = worker.installed_schema_version() else {
                return;
            };
            minimum_version = Some(
                minimum_version
                    .map(|minimum| minimum.min(installed_version))
                    .unwrap_or(installed_version),
            );
        }
        if let Some(minimum_version) = minimum_version {
            self.mark_schema_version_published(minimum_version);
        }
    }

    fn install_request(&self) -> &RuntimeSinkInstallRequest {
        &self.install_request
    }

    fn resolved(&self) -> &ResolvedRuntimePlugin {
        &self.resolved
    }

    fn pipeline_name(&self) -> &str {
        &self.pipeline_name
    }
}

impl Drop for RuntimeSinkConnectionPool {
    fn drop(&mut self) {
        if let Some(abort) = self
            .maintenance_abort
            .lock()
            .expect("runtime sink maintenance task poisoned")
            .take()
        {
            abort.abort();
        }
    }
}

pub struct RuntimeDataSinkPlugin {
    install_request: RuntimeSinkInstallRequest,
    capability: &'static cdc::SinkCapability,
    pool: Arc<RuntimeSinkConnectionPool>,
}

impl RuntimeDataSinkPlugin {
    #[doc(hidden)]
    pub fn worker_count_for_test(&self) -> usize {
        self.pool.worker_count.load(Ordering::Acquire)
    }

    #[doc(hidden)]
    pub fn session_capacity_for_test(&self) -> usize {
        self.pool.slots.capacity_per_worker
    }

    pub async fn new(
        resolved: ResolvedRuntimePlugin,
        pipeline_name: String,
        binding: RuntimeBinding,
        config: RuntimeSinkConfig,
    ) -> io::Result<Self> {
        let process_budget = RuntimeSinkProcessBudget::for_pipeline(&pipeline_name, 1);
        Self::new_with_process_budget(resolved, pipeline_name, binding, config, process_budget)
            .await
    }

    pub(crate) async fn new_with_process_budget(
        resolved: ResolvedRuntimePlugin,
        pipeline_name: String,
        binding: RuntimeBinding,
        config: RuntimeSinkConfig,
        process_budget: Arc<RuntimeSinkProcessBudget>,
    ) -> io::Result<Self> {
        let capability = resolved
            .manifest
            .sink_capability
            .as_ref()
            .map(|capability| {
                Box::leak(Box::new(capability.to_cdc_capability())) as &'static cdc::SinkCapability
            })
            .ok_or_else(|| {
                io::Error::other(format!(
                    "runtime sink manifest '{}' is missing sink capability",
                    resolved.manifest.name
                ))
            })?;
        let install_request = RuntimeSinkInstallRequest {
            context: runtime_execution_context(&pipeline_name, RuntimeExecutionMode::Sync),
            binding,
            config,
        };
        let pool = RuntimeSinkConnectionPool::new(
            resolved.clone(),
            pipeline_name.clone(),
            install_request.clone(),
            process_budget,
            capability.max_sessions_per_child,
        )
        .await?;
        let plugin = Self {
            install_request,
            capability,
            pool,
        };
        plugin.pool.refresh_published_schema_version().await;
        Ok(plugin)
    }

    async fn restart_worker_inner(
        &self,
        worker: &Arc<RuntimeSinkWorker>,
    ) -> io::Result<Arc<RuntimeSinkMultiplexConnection>> {
        let _restart_guard = worker.restart_lock.lock().await;
        let current = worker.connection().await;
        if current.is_alive() {
            return Ok(current);
        }
        let (replacement, schema_version) = spawn_installed_runtime_sink_connection(
            self.pool.resolved(),
            self.pool.pipeline_name(),
            self.pool.install_request(),
            self.pool.slots.capacity_per_worker,
        )
        .await?;
        worker.set_installed_schema_version(schema_version);
        *worker.connection.lock().await = Arc::clone(&replacement);
        Ok(replacement)
    }

    async fn restart_worker(
        &self,
        worker: &Arc<RuntimeSinkWorker>,
    ) -> io::Result<Arc<RuntimeSinkMultiplexConnection>> {
        let _publish_guard = self.pool.schema_publish_lock.lock().await;
        let _apply_guard = Arc::clone(&self.pool.schema_apply_fence)
            .write_owned()
            .await;
        self.restart_worker_inner(worker).await
    }

    async fn ensure_worker_connection(
        &self,
        worker: &Arc<RuntimeSinkWorker>,
    ) -> io::Result<Arc<RuntimeSinkMultiplexConnection>> {
        let connection = worker.connection().await;
        if connection.is_alive() {
            Ok(connection)
        } else {
            self.restart_worker(worker).await
        }
    }

    async fn send_schema_state_install(
        &self,
        worker: &Arc<RuntimeSinkWorker>,
        connection: &Arc<RuntimeSinkMultiplexConnection>,
        schema_version: u64,
        namespaces: &BTreeMap<String, OutputMetadata>,
    ) -> io::Result<()> {
        if worker
            .installed_schema_version()
            .is_some_and(|installed| installed >= schema_version)
        {
            crate::metrics::counters::add_runtime_schema_state_publication_skipped(1);
            return Ok(());
        }
        let frame = timeout(
            runtime_sink_schema_timeout(),
            connection.send_admin(&HostFrame::InstallSchemaState(
                RuntimeSchemaStateInstallRequest {
                    schema_state: RuntimeSchemaState {
                        version: schema_version,
                        namespaces: namespaces.clone(),
                    },
                },
            )),
        )
        .await
        .map_err(|_| {
            connection.shared.fail(
                ErrorKind::TimedOut,
                "runtime sink schema state install timed out",
            );
            io::Error::new(
                ErrorKind::TimedOut,
                "runtime sink schema state install timed out",
            )
        })??;
        match frame {
            PluginFrame::Installed => {
                worker.set_installed_schema_version(schema_version);
                Ok(())
            }
            other => Err(io::Error::new(
                ErrorKind::InvalidData,
                format!("unexpected runtime sink schema install response: {other:?}"),
            )),
        }
    }

    async fn ensure_latest_schema_state(&self) -> io::Result<u64> {
        let required_version = current_pipeline_schema_version();
        if self.pool.has_published_schema_version(required_version) {
            self.pool.record_schema_publication_skipped();
            return Ok(required_version);
        }
        let schema_state = current_runtime_schema_state();
        self.install_schema_state_with_retry(schema_state.version, &schema_state.namespaces)
            .await?;
        Ok(schema_state.version)
    }

    async fn send_sink_payload(
        connection: &RuntimeSinkMultiplexConnection,
        request_id: u64,
        arrow_stream_bytes: &[u8],
        rows: u64,
    ) -> io::Result<()> {
        let chunk_size = runtime_sink_payload_chunk_bytes();
        let mut chunks = 0u32;
        for (chunk_index, chunk) in arrow_stream_bytes.chunks(chunk_size).enumerate() {
            let sink_chunk = SinkChunk {
                request_id,
                chunk_index: chunk_index as u32,
                row_offset: 0,
                rows: if chunk_index == 0 { rows } else { 0 },
                arrow_stream_bytes: chunk.to_vec(),
            };
            sink_chunk.validate_bound().map_err(io::Error::other)?;
            connection
                .send_data(&HostDataFrame::SinkChunk(sink_chunk))
                .await?;
            chunks = chunks.saturating_add(1);
            tokio::task::yield_now().await;
        }
        connection
            .send_data(&HostDataFrame::FinishSink(FinishSink {
                request_id,
                chunks,
                rows,
                bytes: arrow_stream_bytes.len() as u64,
            }))
            .await
    }

    async fn send_sink_request(
        &self,
        request: SinkRunRequest,
        stream: datafusion::execution::SendableRecordBatchStream,
    ) -> io::Result<SinkWriteOutcome> {
        let mut retried = false;
        let mut schema_refreshes = 0usize;
        let mut stream = Some(stream);
        let mut encoded_bytes: Option<Vec<u8>> = None;
        let mut encoded_rows: Option<u64> = None;
        let prepare = PrepareSink {
            request_id: request.request_id,
            envelope: sink_apply_envelope(&request),
            request: request.clone(),
        };
        loop {
            let lease = self.pool.acquire().await?;
            let worker = Arc::clone(&lease.worker);
            let connection = self.ensure_worker_connection(&worker).await?;
            let apply_guard = Arc::clone(&self.pool.schema_apply_fence).read_owned().await;
            let mut session = connection.register_session(request.request_id)?;
            let request_timeout = runtime_sink_request_timeout();
            let mut payload_started = false;
            let response = timeout(request_timeout, async {
                connection
                    .send_control(&HostFrame::PrepareSink(prepare.clone()))
                    .await?;
                let prepare_response = session.recv().await?;
                match prepare_response {
                    PluginFrame::PrepareAck(PrepareAck {
                        request_id,
                        result: PrepareSinkResult::Ready,
                    }) if request_id == request.request_id => {
                        if encoded_bytes.is_none() {
                            let source_stream = stream.take().ok_or_else(|| {
                                io::Error::other(
                                    "runtime sink stream was consumed before payload encoding",
                                )
                            })?;
                            let encoded =
                                encode_record_batch_stream_with_stats(source_stream).await?;
                            encoded_rows = Some(encoded.rows);
                            encoded_bytes = Some(encoded.bytes);
                        }
                        payload_started = true;
                        Self::send_sink_payload(
                            &connection,
                            request.request_id,
                            encoded_bytes
                                .as_deref()
                                .expect("runtime sink payload encoded"),
                            encoded_rows.expect("runtime sink row count encoded"),
                        )
                        .await?;
                        session.recv().await
                    }
                    other => Ok(other),
                }
            })
            .await;
            drop(session);
            drop(apply_guard);
            drop(lease);

            let frame = match response {
                Ok(Ok(frame)) => frame,
                Ok(Err(err)) => {
                    let retry = !payload_started && !retried;
                    let _ = self.restart_worker(&worker).await;
                    if retry {
                        retried = true;
                        continue;
                    }
                    return Err(err);
                }
                Err(_) => {
                    let err = io::Error::new(
                        ErrorKind::TimedOut,
                        format!(
                            "runtime sink request {} timed out after {}s",
                            request.request_id,
                            request_timeout.as_secs()
                        ),
                    );
                    warn!(
                        "runtime sink request timed out request_id={} compaction_id={} timeout_secs={}",
                        request.request_id,
                        request.compaction_id,
                        request_timeout.as_secs()
                    );
                    connection.shared.fail(err.kind(), err.to_string());
                    let retry = !payload_started && !retried;
                    let _ = self.restart_worker(&worker).await;
                    if retry {
                        retried = true;
                        continue;
                    }
                    return Err(err);
                }
            };

            match frame {
                PluginFrame::PrepareAck(PrepareAck {
                    request_id,
                    result: PrepareSinkResult::AlreadyApplied(receipt),
                }) if request_id == request.request_id => {
                    validate_commit_receipt(&receipt, &prepare.envelope)?;
                    return Ok(SinkWriteOutcome::AlreadyApplied);
                }
                PluginFrame::PrepareAck(PrepareAck {
                    request_id,
                    result: PrepareSinkResult::Rejected { reason },
                }) if request_id == request.request_id => {
                    return Err(io::Error::other(format!(
                        "runtime sink prepare rejected: {reason}"
                    )));
                }
                PluginFrame::SinkAck(ack) if ack.request_id == request.request_id => {
                    if let Err(err) = validate_sink_ack(&ack, &prepare.envelope) {
                        connection.shared.fail(err.kind(), err.to_string());
                        let _ = self.restart_worker(&worker).await;
                        return Err(err);
                    }
                    if let Some(rows) = ack.stats.rows {
                        crate::metrics::counters::add_parquet_rows(rows);
                    }
                    return Ok(ack.outcome);
                }
                PluginFrame::SinkError(RuntimeSinkError {
                    request_id,
                    message,
                }) if request_id == request.request_id => {
                    return Err(io::Error::other(message));
                }
                PluginFrame::SchemaStateRefreshRequired(refresh)
                    if refresh.request_id == request.request_id && !payload_started =>
                {
                    if schema_refreshes >= 3 {
                        return Err(io::Error::other(
                            "runtime sink repeatedly requested schema refresh",
                        ));
                    }
                    worker.has_installed_schema.store(false, Ordering::Release);
                    let schema_state = current_runtime_schema_state();
                    self.install_schema_state_with_retry(
                        schema_state.version,
                        &schema_state.namespaces,
                    )
                    .await?;
                    schema_refreshes += 1;
                    continue;
                }
                other => {
                    let err = io::Error::new(
                        ErrorKind::InvalidData,
                        format!(
                            "unexpected runtime sink response for request {}: {:?}",
                            request.request_id, other
                        ),
                    );
                    connection.shared.fail(err.kind(), err.to_string());
                    let retry = !payload_started && !retried;
                    let _ = self.restart_worker(&worker).await;
                    if retry {
                        retried = true;
                        continue;
                    }
                    return Err(err);
                }
            }
        }
    }

    async fn install_schema_state_with_retry(
        &self,
        schema_version: u64,
        namespaces: &BTreeMap<String, OutputMetadata>,
    ) -> io::Result<()> {
        if self.pool.has_published_schema_version(schema_version) {
            self.pool.record_schema_publication_skipped();
            return Ok(());
        }
        let _publish_guard = self.pool.schema_publish_lock.lock().await;
        if self.pool.has_published_schema_version(schema_version) {
            self.pool.record_schema_publication_skipped();
            return Ok(());
        }
        let _apply_guard = Arc::clone(&self.pool.schema_apply_fence)
            .write_owned()
            .await;
        let workers = {
            let workers = self.pool.workers.lock().await;
            workers.iter().cloned().collect::<Vec<_>>()
        };
        for worker in workers {
            let mut connection = worker.connection().await;
            if !connection.is_alive() {
                connection = self.restart_worker_inner(&worker).await?;
            }
            if let Err(err) = self
                .send_schema_state_install(&worker, &connection, schema_version, namespaces)
                .await
            {
                warn!(
                    "runtime sink schema state install failed, restarting child: {}",
                    err
                );
                connection = self.restart_worker_inner(&worker).await?;
                self.send_schema_state_install(&worker, &connection, schema_version, namespaces)
                    .await?;
            }
        }
        self.pool.mark_schema_version_published(schema_version);
        Ok(())
    }

    async fn send_grouped_sink_request(
        &self,
        request: SinkRunRequest,
        mut reader: crate::plugins::GroupedBatchReader,
    ) -> io::Result<SinkWriteOutcome> {
        let request_id = request.request_id;
        let compaction_id = request.compaction_id.clone();
        let prepare = PrepareSink {
            request_id,
            envelope: sink_apply_envelope(&request),
            request,
        };
        let mut schema_refreshes = 0usize;
        let mut retried = false;
        loop {
            let lease = self.pool.acquire().await?;
            let worker = Arc::clone(&lease.worker);
            let connection = self.ensure_worker_connection(&worker).await?;
            let apply_guard = Arc::clone(&self.pool.schema_apply_fence).read_owned().await;
            let mut session = connection.register_session(request_id)?;
            let request_timeout = runtime_sink_request_timeout();
            let mut payload_started = false;
            let response = timeout(request_timeout, async {
                connection
                    .send_control(&HostFrame::PrepareSink(prepare.clone()))
                    .await?;
                let prepare_response = session.recv().await?;
                if !matches!(
                    prepare_response,
                    PluginFrame::PrepareAck(PrepareAck {
                        request_id: ack_id,
                        result: PrepareSinkResult::Ready,
                    }) if ack_id == request_id
                ) {
                    return Ok::<_, io::Error>(prepare_response);
                }
                let mut sent_chunks = 0u32;
                let mut total_rows = 0u64;
                let mut total_bytes = 0u64;
                while let Some(chunk) = reader.next_chunk().await? {
                    let arrow_stream_bytes = encode_record_batches(&chunk.batches)?;
                    let sink_chunk = SinkChunk {
                        request_id,
                        chunk_index: sent_chunks,
                        row_offset: chunk.row_offset,
                        rows: chunk.rows,
                        arrow_stream_bytes,
                    };
                    sink_chunk.validate_bound().map_err(io::Error::other)?;
                    total_rows = total_rows.saturating_add(sink_chunk.rows);
                    total_bytes =
                        total_bytes.saturating_add(sink_chunk.arrow_stream_bytes.len() as u64);
                    payload_started = true;
                    connection
                        .send_data(&HostDataFrame::SinkChunk(sink_chunk))
                        .await?;
                    sent_chunks = sent_chunks.saturating_add(1);
                    tokio::task::yield_now().await;
                }
                if sent_chunks == 0 {
                    let sink_chunk = SinkChunk {
                        request_id,
                        chunk_index: 0,
                        row_offset: 0,
                        rows: 0,
                        arrow_stream_bytes: encode_record_batches(&[])?,
                    };
                    sink_chunk.validate_bound().map_err(io::Error::other)?;
                    total_bytes = sink_chunk.arrow_stream_bytes.len() as u64;
                    payload_started = true;
                    connection
                        .send_data(&HostDataFrame::SinkChunk(sink_chunk))
                        .await?;
                    sent_chunks = 1;
                }
                connection
                    .send_data(&HostDataFrame::FinishSink(FinishSink {
                        request_id,
                        chunks: sent_chunks,
                        rows: total_rows,
                        bytes: total_bytes,
                    }))
                    .await?;
                session.recv().await
            })
            .await;
            drop(session);
            drop(apply_guard);
            drop(lease);
            let frame = match response {
                Ok(Ok(frame)) => frame,
                Ok(Err(err)) => {
                    let retry = !payload_started && !retried;
                    let _ = self.restart_worker(&worker).await;
                    if retry {
                        retried = true;
                        continue;
                    }
                    return Err(err);
                }
                Err(_) => {
                    let err = io::Error::new(
                        ErrorKind::TimedOut,
                        format!(
                            "runtime grouped sink request {} timed out after {}s",
                            request_id,
                            request_timeout.as_secs()
                        ),
                    );
                    warn!(
                        "runtime grouped sink request timed out request_id={} compaction_id={} timeout_secs={}",
                        request_id,
                        compaction_id,
                        request_timeout.as_secs()
                    );
                    connection.shared.fail(err.kind(), err.to_string());
                    let retry = !payload_started && !retried;
                    let _ = self.restart_worker(&worker).await;
                    if retry {
                        retried = true;
                        continue;
                    }
                    return Err(err);
                }
            };
            match frame {
                PluginFrame::PrepareAck(PrepareAck {
                    request_id: ack_id,
                    result: PrepareSinkResult::AlreadyApplied(receipt),
                }) if ack_id == request_id => {
                    validate_commit_receipt(&receipt, &prepare.envelope)?;
                    return Ok(SinkWriteOutcome::AlreadyApplied);
                }
                PluginFrame::PrepareAck(PrepareAck {
                    request_id: ack_id,
                    result: PrepareSinkResult::Rejected { reason },
                }) if ack_id == request_id => {
                    return Err(io::Error::other(format!(
                        "runtime grouped sink prepare rejected: {reason}"
                    )));
                }
                PluginFrame::SchemaStateRefreshRequired(refresh)
                    if refresh.request_id == request_id && !payload_started =>
                {
                    if schema_refreshes >= 3 {
                        return Err(io::Error::other(
                            "runtime grouped sink repeatedly requested schema refresh",
                        ));
                    }
                    worker.has_installed_schema.store(false, Ordering::Release);
                    let schema_state = current_runtime_schema_state();
                    self.install_schema_state_with_retry(
                        schema_state.version,
                        &schema_state.namespaces,
                    )
                    .await?;
                    schema_refreshes += 1;
                    continue;
                }
                PluginFrame::SinkAck(ack) if ack.request_id == request_id => {
                    if let Err(err) = validate_sink_ack(&ack, &prepare.envelope) {
                        connection.shared.fail(err.kind(), err.to_string());
                        let _ = self.restart_worker(&worker).await;
                        return Err(err);
                    }
                    if let Some(rows) = ack.stats.rows {
                        crate::metrics::counters::add_parquet_rows(rows);
                    }
                    return Ok(ack.outcome);
                }
                PluginFrame::SinkError(RuntimeSinkError {
                    request_id: error_request_id,
                    message,
                }) if error_request_id == request_id => {
                    return Err(io::Error::other(message));
                }
                other => {
                    let err = io::Error::new(
                        ErrorKind::InvalidData,
                        format!(
                            "unexpected runtime grouped sink response for request {}: {:?}",
                            request_id, other
                        ),
                    );
                    connection.shared.fail(err.kind(), err.to_string());
                    let retry = !payload_started && !retried;
                    let _ = self.restart_worker(&worker).await;
                    if retry {
                        retried = true;
                        continue;
                    }
                    return Err(err);
                }
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
        self.sync_with_context(
            stream,
            crate::plugins::SinkWriteContext {
                filename,
                compaction_id: String::new(),
                idempotency_key: String::new(),
                wal_refs: Vec::new(),
                write_semantics:
                    crate::buffer::compaction_transaction::SinkWriteSemantics::AtLeastOnce,
                schema_fingerprint: String::new(),
                cdc_ctx,
                source_contract: None,
            },
        )
        .await
    }

    async fn sync_with_context(
        &self,
        stream: datafusion::execution::SendableRecordBatchStream,
        ctx: crate::plugins::SinkWriteContext<'_>,
    ) -> Result<(), io::Error> {
        self.sync_with_context_result(stream, ctx).await.map(|_| ())
    }

    async fn sync_with_context_result(
        &self,
        stream: datafusion::execution::SendableRecordBatchStream,
        ctx: crate::plugins::SinkWriteContext<'_>,
    ) -> Result<SinkWriteOutcome, io::Error> {
        if let Some(namespace) = query_value_from_runtime_filename(&ctx.filename, "namespace") {
            let schema = stream.schema();
            if !schema.fields().is_empty() {
                apply_derived_runtime_schema(namespace, &schema);
            }
        }
        let schema_version = self.ensure_latest_schema_state().await?;
        let namespace = query_value_from_runtime_filename(&ctx.filename, "namespace");
        let source_contract = ctx.source_contract.cloned().or_else(|| {
            namespace
                .as_deref()
                .and_then(crate::plugins::source_contract::namespace_source_contract)
        });
        let request = SinkRunRequest {
            request_id: next_runtime_request_id(),
            compaction_id: if ctx.compaction_id.is_empty() {
                runtime_compaction_id(&ctx.filename)
            } else {
                ctx.compaction_id.clone()
            },
            idempotency_key: if ctx.idempotency_key.is_empty() {
                if ctx.compaction_id.is_empty() {
                    runtime_compaction_id(&ctx.filename)
                } else {
                    ctx.compaction_id.clone()
                }
            } else {
                ctx.idempotency_key.clone()
            },
            wal_refs: ctx.wal_refs.clone(),
            write_semantics: ctx.write_semantics,
            schema_fingerprint: ctx.schema_fingerprint.clone(),
            binding: self.install_request.binding,
            required_schema_version: schema_version,
            filename: ctx.filename,
            cdc_ctx: ctx.cdc_ctx.cloned(),
            source_contract,
            payload_mode: RuntimeSinkPayloadMode::FullStream,
        };
        self.send_sink_request(request, stream).await
    }

    async fn sync_grouped(
        &self,
        reader: crate::plugins::GroupedBatchReader,
        ctx: crate::plugins::GroupedSinkWriteContext<'_>,
    ) -> Result<SinkWriteOutcome, io::Error> {
        if let Some(namespace) = query_value_from_runtime_filename(&ctx.filename, "namespace") {
            let schema = reader.schema();
            if !schema.fields().is_empty() {
                apply_derived_runtime_schema(namespace, &schema);
            }
        }
        let schema_version = self.ensure_latest_schema_state().await?;
        let namespace = query_value_from_runtime_filename(&ctx.filename, "namespace");
        let source_contract = ctx.source_contract.cloned().or_else(|| {
            namespace
                .as_deref()
                .and_then(crate::plugins::source_contract::namespace_source_contract)
        });
        let request = SinkRunRequest {
            request_id: next_runtime_request_id(),
            compaction_id: if ctx.compaction_id.is_empty() {
                runtime_compaction_id(&ctx.filename)
            } else {
                ctx.compaction_id.clone()
            },
            idempotency_key: if ctx.idempotency_key.is_empty() {
                if ctx.compaction_id.is_empty() {
                    runtime_compaction_id(&ctx.filename)
                } else {
                    ctx.compaction_id.clone()
                }
            } else {
                ctx.idempotency_key.clone()
            },
            wal_refs: ctx.wal_refs.clone_vec(),
            write_semantics: ctx.write_semantics,
            schema_fingerprint: ctx.schema_fingerprint.clone(),
            binding: self.install_request.binding,
            required_schema_version: schema_version,
            filename: ctx.filename,
            cdc_ctx: ctx.cdc_ctx.cloned(),
            source_contract,
            payload_mode: RuntimeSinkPayloadMode::GroupedChunks,
        };
        self.send_grouped_sink_request(request, reader).await
    }

    fn capability(&self) -> &'static cdc::SinkCapability {
        self.capability
    }

    async fn install_schema_state(
        &self,
        schema_version: u64,
        namespaces: &BTreeMap<String, OutputMetadata>,
    ) -> Result<(), io::Error> {
        if self.pool.has_published_schema_version(schema_version) {
            self.pool.record_schema_publication_skipped();
            return Ok(());
        }
        apply_runtime_source_schema_state(RuntimeSchemaState {
            version: schema_version,
            namespaces: namespaces.clone(),
        });
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
            context: runtime_execution_context(&pipeline_name, RuntimeExecutionMode::Sync),
            binding,
            config,
        };
        let connection =
            RuntimeChildConnection::spawn(resolved, pipeline_name, None, None, 1).await?;
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
        if connection
            .installed_schema_version
            .is_some_and(|installed| installed >= schema_version)
        {
            crate::metrics::counters::add_runtime_schema_state_publication_skipped(1);
            return Ok(());
        }
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
        expect_install_ack(connection, "schema state").await?;
        connection.installed_schema_version = Some(schema_version);
        Ok(())
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
                    guard.installed_schema_version = None;
                    self.install_latest_schema_state(&mut guard).await?;
                    schema_refreshes += 1;
                    continue;
                }
                Ok(other) => {
                    let err =
                        io::Error::other(format!("unexpected runtime schema frame: {:?}", other));
                    if !retried && should_retry_runtime_connection(&err) {
                        warn!(
                            "runtime schema request got stale frame, restarting child: {}",
                            err
                        );
                        self.restart_and_reinstall(&mut guard).await?;
                        retried = true;
                        continue;
                    }
                    return Err(err);
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
        metadata: &crate::discover::OutputMetadata,
    ) -> Result<(), io::Error> {
        self.sync_schema_request(
            SchemaSyncRequest {
                namespace,
                compaction_id: "",
                source_contract: None,
            },
            metadata,
        )
        .await
    }

    async fn sync_schema_request(
        &self,
        request_ctx: SchemaSyncRequest<'_>,
        metadata: &crate::discover::OutputMetadata,
    ) -> Result<(), io::Error> {
        let schema_version = current_pipeline_schema_version();
        let schema_fingerprint = runtime_schema_compaction_fingerprint(metadata);
        let request = SchemaRunRequest {
            request_id: next_runtime_request_id(),
            compaction_id: if request_ctx.compaction_id.is_empty() {
                runtime_schema_compaction_id(
                    self.install_request.binding,
                    &schema_fingerprint,
                    request_ctx.namespace,
                )
            } else {
                request_ctx.compaction_id.to_string()
            },
            binding: self.install_request.binding,
            required_schema_version: schema_version,
            namespace: request_ctx.namespace.to_string(),
            source_contract: request_ctx.source_contract.cloned().or_else(|| {
                crate::plugins::source_contract::namespace_source_contract(request_ctx.namespace)
            }),
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
    use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

    use super::{
        handle_runtime_offset_request, reset_runtime_sink_process_budgets_for_test,
        BufferedRuntimeFrameReader, RuntimeChildConnection, RuntimeSinkProcessBudget,
        MAX_RUNTIME_FRAME_BYTES,
    };
    use crate::helpers::configuration::Config;
    use crate::helpers::offsets::{
        OffsetKey, OffsetTypes, Offsets, RuntimeOffsetOperation, RuntimeOffsetRpcRequest,
        RuntimeOffsetValue,
    };
    use crate::plugins::cdc::{CheckpointAuthority, CheckpointEnvelope, CheckpointKind};
    use crate::runtime_plugins::protocol::RuntimeBinding;
    use crate::runtime_plugins::wire::write_frame;
    use serde::{Deserialize, Serialize};
    use serial_test::serial;
    use tempfile::TempDir;
    use tokio::net::{TcpListener, TcpStream};

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

    #[tokio::test]
    async fn runtime_channel_rejects_protocol_v16_before_handshake() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accept = tokio::spawn(async move {
            RuntimeChildConnection::accept_runtime_channel(&listener, "v17-token").await
        });
        let mut child = TcpStream::connect(addr).await.unwrap();
        write_frame(
            &mut child,
            &crate::runtime_plugins::protocol::RuntimeSessionHello {
                protocol_version: 16,
                token: "v17-token".to_string(),
            },
        )
        .await
        .unwrap();
        let err = accept.await.unwrap().unwrap_err();
        assert!(err.to_string().contains("host=17 child=16"));
    }

    #[test]
    fn runtime_sink_budget_caps_and_balances_two_bindings() {
        let budget = RuntimeSinkProcessBudget::new_for_test(8, 4, 2);
        let primary = budget.register(RuntimeBinding::Primary, 2);
        let deadletter = budget.register(RuntimeBinding::Deadletter, 2);
        let mut primary_permits = vec![primary.acquire_initial().unwrap()];
        let mut deadletter_permits = vec![deadletter.acquire_initial().unwrap()];

        while let Some(permit) = primary.try_acquire_additional() {
            primary_permits.push(permit);
        }
        while let Some(permit) = deadletter.try_acquire_additional() {
            deadletter_permits.push(permit);
        }

        assert_eq!(primary.active_workers(), 2);
        assert_eq!(deadletter.active_workers(), 2);
        assert_eq!(budget.active_workers(), 4);
        assert!(primary.try_acquire_additional().is_none());
        assert!(deadletter.try_acquire_additional().is_none());

        drop(primary_permits);
        drop(deadletter_permits);
        assert_eq!(budget.active_workers(), 0);
    }

    #[test]
    fn runtime_sink_budget_clamps_below_binding_count() {
        let budget = RuntimeSinkProcessBudget::new_for_test(1, 1, 2);
        let primary = budget.register(RuntimeBinding::Primary, 2);
        let deadletter = budget.register(RuntimeBinding::Deadletter, 2);
        let primary_permit = primary.acquire_initial().unwrap();
        let deadletter_permit = deadletter.acquire_initial().unwrap();

        assert_eq!(primary.active_workers(), 1);
        assert_eq!(deadletter.active_workers(), 1);
        assert_eq!(budget.active_workers(), 2);
        assert!(primary.try_acquire_additional().is_none());
        assert!(deadletter.try_acquire_additional().is_none());

        drop(primary_permit);
        drop(deadletter_permit);
        drop(primary);
        drop(deadletter);
        assert_eq!(budget.active_workers(), 0);
    }

    #[test]
    fn runtime_sink_budget_restart_and_drop_accounting_is_stable() {
        let budget = RuntimeSinkProcessBudget::new_for_test(1, 1, 1);
        let binding = budget.register(RuntimeBinding::Primary, 1);
        let worker_permit = binding.acquire_initial().unwrap();

        // A child restart replaces only RuntimeChildConnection; its worker-owned permit stays put.
        assert_eq!(binding.active_workers(), 1);
        assert!(binding.try_acquire_additional().is_none());
        assert_eq!(budget.active_workers(), 1);

        drop(binding);
        assert_eq!(budget.active_workers(), 1);
        drop(worker_permit);
        assert_eq!(budget.active_workers(), 0);
    }

    #[test]
    fn runtime_sink_budget_registry_can_reset_without_leaking_state() {
        let budget = RuntimeSinkProcessBudget::for_pipeline("budget-reset-test", 1);
        let binding = budget.register(RuntimeBinding::Primary, 1);
        let permit = binding.acquire_initial().unwrap();
        reset_runtime_sink_process_budgets_for_test();

        let replacement = RuntimeSinkProcessBudget::for_pipeline("budget-reset-test", 1);
        assert!(!Arc::ptr_eq(&budget, &replacement));
        assert_eq!(replacement.active_workers(), 0);

        drop(permit);
        drop(binding);
        assert_eq!(budget.active_workers(), 0);
    }

    #[test]
    fn runtime_sink_budget_derives_processes_from_session_capacity() {
        let budget = RuntimeSinkProcessBudget::new_for_test(5, 16, 1);
        let binding = budget.register(RuntimeBinding::Primary, 2);
        let mut permits = vec![binding.acquire_initial().unwrap()];
        while let Some(permit) = binding.try_acquire_additional() {
            permits.push(permit);
        }
        assert_eq!(binding.desired_workers(), 3);
        assert_eq!(binding.active_workers(), 3);
        drop(permits);
    }

    #[test]
    fn runtime_channel_reset_errors_are_treated_as_eof() {
        for kind in [
            std::io::ErrorKind::ConnectionAborted,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::UnexpectedEof,
        ] {
            let err = std::io::Error::new(kind, "closed");
            assert!(super::is_runtime_channel_eof(&err));
        }

        let err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        assert!(!super::is_runtime_channel_eof(&err));
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
