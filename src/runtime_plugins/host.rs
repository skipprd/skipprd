use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tracing::warn;

use crate::buffer::ingest_buffer::flush_all_segments;
use crate::helpers::configuration::Config;
use crate::helpers::offsets::Offsets;
use crate::ingest_work::{Ingest, IngestBatch, IngestTask, IngestTasks};
use crate::plugins::cdc::{self, CheckpointAuthority, CheckpointEnvelope};
use crate::plugins::{DataSink, SchemaSink};
use crate::runtime_plugins::artifact::resolve_plugin_executable;
use crate::runtime_plugins::framing::{read_plugin_frame, write_host_frame};
use crate::runtime_plugins::manifest::RuntimePluginManifest;
use crate::runtime_plugins::protocol::{
    HandshakeRequest, HostFrame, PluginFrame, RuntimeBinding, RuntimeCheckpointUpdate,
    RuntimeSchemaConfig, RuntimeSinkConfig, RuntimeSourceConfig, SchemaRunRequest, SinkRunRequest,
    SourceEvent, SourceStartRequest, RUNTIME_PROTOCOL_VERSION,
};
use crate::runtime_plugins::sdk::{decode_record_batch_stream, encode_record_batch_stream};

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
    stdin: ChildStdin,
    stdout: ChildStdout,
}

impl RuntimeChildConnection {
    async fn spawn(resolved: ResolvedRuntimePlugin, pipeline_name: String) -> io::Result<Self> {
        let executable =
            resolve_plugin_executable(&resolved.manifest_path, &resolved.manifest).await?;
        let mut command = Command::new(executable);
        command
            .args(&resolved.manifest.args)
            .env("PIPELINE_NAME", &pipeline_name)
            .env("WORKSPACE_NAME", Config::get_workspace_name())
            .env("DATA_DIR", Config::get_pipeline_data_dir())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());

        let mut child = command.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("runtime child stdin was not piped"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("runtime child stdout was not piped"))?;
        let mut connection = Self {
            resolved,
            pipeline_name,
            child,
            stdin,
            stdout,
        };
        connection.handshake().await?;
        Ok(connection)
    }

    async fn restart(&mut self) -> io::Result<()> {
        let _ = self.child.kill().await;
        let replacement = Self::spawn(self.resolved.clone(), self.pipeline_name.clone()).await?;
        *self = replacement;
        Ok(())
    }

    async fn handshake(&mut self) -> io::Result<()> {
        let request = HostFrame::Handshake(HandshakeRequest {
            pipeline_name: self.pipeline_name.clone(),
            protocol_version: RUNTIME_PROTOCOL_VERSION,
        });
        write_host_frame(&mut self.stdin, &request).await?;
        let frame = read_plugin_frame(&mut self.stdout).await?;
        let PluginFrame::HandshakeAck(handshake) = frame else {
            return Err(io::Error::other(
                "runtime plugin did not respond with a handshake ack",
            ));
        };
        self.resolved.manifest.verify_handshake(&handshake)
    }

    fn has_exited(&mut self) -> io::Result<bool> {
        Ok(self.child.try_wait()?.is_some())
    }

    async fn send(&mut self, frame: &HostFrame) -> io::Result<()> {
        write_host_frame(&mut self.stdin, frame).await
    }

    async fn recv(&mut self) -> io::Result<PluginFrame> {
        read_plugin_frame(&mut self.stdout).await
    }
}

fn checkpoint_envelope_key(key: &str) -> String {
    format!("runtime_checkpoint::{key}")
}

fn load_checkpoint_envelope(offsets: &Offsets, key: &str) -> Option<CheckpointEnvelope> {
    offsets
        .load_checkpoint(&checkpoint_envelope_key(key))
        .and_then(|bytes| bincode::deserialize(&bytes).ok())
}

fn store_checkpoint_update(offsets: &Offsets, update: &RuntimeCheckpointUpdate) -> io::Result<()> {
    let envelope_bytes =
        bincode::serialize(&update.envelope).map_err(|err| io::Error::other(err.to_string()))?;
    offsets.store_checkpoint(&checkpoint_envelope_key(&update.key), &envelope_bytes);
    if let Some(legacy) = update.legacy_payload_bytes.as_ref() {
        offsets.store_checkpoint(&update.key, legacy);
    }
    Ok(())
}

fn build_source_start_request_for_pipeline(
    plugin_name: &str,
    offsets: &Offsets,
) -> io::Result<SourceStartRequest> {
    let source_config = RuntimeSourceConfig::try_from(
        Config::get_pipeline_input_plugin_config().map_err(io::Error::other)?,
    )
    .map_err(io::Error::other)?;

    if plugin_name == "Postgres" {
        let raw_source_config: serde_json::Value =
            source_config.0.decode().map_err(io::Error::other)?;
        let slot_name = raw_source_config
            .as_object()
            .and_then(|config| config.get("replication_slot_name"))
            .and_then(|value| value.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| "skippr_slot".to_string());
        let resume_key = format!("postgres:{slot_name}:lsn");
        let anchor_key = format!("postgres:{slot_name}:bootstrap_anchor");
        Ok(SourceStartRequest {
            config: source_config,
            resume_checkpoint: load_checkpoint_envelope(offsets, &resume_key),
            legacy_resume_bytes: offsets.load_checkpoint(&resume_key),
            bootstrap_anchor: load_checkpoint_envelope(offsets, &anchor_key),
        })
    } else {
        Ok(SourceStartRequest {
            config: source_config,
            resume_checkpoint: None,
            legacy_resume_bytes: None,
            bootstrap_anchor: None,
        })
    }
}

fn ingest_batches_into_core(
    batches: Vec<IngestBatch>,
    offsets: Arc<Offsets>,
    shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
) {
    if batches.is_empty() {
        return;
    }
    let ingest = Ingest::new();
    let mut ingest_tasks = IngestTasks::new();
    ingest_tasks.add(IngestTask::new(
        batches,
        offsets.clone(),
        shared_output.clone(),
    ));
    ingest.ingest_file(&Arc::new(ingest_tasks), &offsets, shared_output);
}

pub async fn sync_runtime_input_plugin(
    resolved: ResolvedRuntimePlugin,
    pipeline_name: String,
    offsets: Arc<Offsets>,
    shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
) -> io::Result<()> {
    let mut connection = RuntimeChildConnection::spawn(resolved, pipeline_name).await?;
    let start_request = build_source_start_request_for_pipeline(
        &connection.resolved.manifest.plugin_name,
        &offsets,
    )?;
    connection
        .send(&HostFrame::RunSource(start_request))
        .await?;

    let mut saw_unflushed_batches = false;
    loop {
        match connection.recv().await? {
            PluginFrame::SourceEvent(SourceEvent::IngestBatches { batches }) => {
                if !batches.is_empty() {
                    ingest_batches_into_core(batches, offsets.clone(), shared_output.clone());
                    saw_unflushed_batches = true;
                }
            }
            PluginFrame::SourceEvent(SourceEvent::SinkWrite {
                filename,
                arrow_stream_bytes,
                cdc_ctx,
            }) => {
                let stream = decode_record_batch_stream(arrow_stream_bytes)?;
                shared_output
                    .sync(stream, filename, cdc_ctx.as_ref())
                    .await?;
            }
            PluginFrame::SourceEvent(SourceEvent::CheckpointUpdate(update)) => {
                if update.envelope.authority == CheckpointAuthority::WalOwnership
                    && saw_unflushed_batches
                {
                    flush_all_segments(offsets.clone())
                        .await
                        .map_err(|err| io::Error::other(err.to_string()))?;
                    saw_unflushed_batches = false;
                }
                store_checkpoint_update(&offsets, &update)?;
            }
            PluginFrame::SourceEvent(SourceEvent::Completed) => {
                if saw_unflushed_batches {
                    flush_all_segments(offsets.clone())
                        .await
                        .map_err(|err| io::Error::other(err.to_string()))?;
                }
                break;
            }
            PluginFrame::Error(err) => return Err(io::Error::other(err)),
            other => {
                return Err(io::Error::other(format!(
                    "unexpected runtime source frame: {:?}",
                    other
                )));
            }
        }
    }

    Ok(())
}

pub struct RuntimeDataSinkPlugin {
    binding: RuntimeBinding,
    config: RuntimeSinkConfig,
    connection: Mutex<RuntimeChildConnection>,
}

impl RuntimeDataSinkPlugin {
    pub async fn new(
        resolved: ResolvedRuntimePlugin,
        pipeline_name: String,
        binding: RuntimeBinding,
        config: RuntimeSinkConfig,
    ) -> io::Result<Self> {
        let connection =
            RuntimeChildConnection::spawn(resolved.clone(), pipeline_name.clone()).await?;
        Ok(Self {
            binding,
            config,
            connection: Mutex::new(connection),
        })
    }

    async fn send_sink_request(&self, request: SinkRunRequest) -> io::Result<()> {
        let mut retried = false;
        loop {
            let mut guard = self.connection.lock().await;
            if guard.has_exited()? {
                guard.restart().await?;
            }

            let send_result = guard.send(&HostFrame::RunSink(request.clone())).await;
            let recv_result = match send_result {
                Ok(_) => guard.recv().await,
                Err(err) => Err(err),
            };

            match recv_result {
                Ok(PluginFrame::SinkAck) => return Ok(()),
                Ok(PluginFrame::Error(err)) => return Err(io::Error::other(err)),
                Ok(other) => {
                    return Err(io::Error::other(format!(
                        "unexpected runtime sink frame: {:?}",
                        other
                    )))
                }
                Err(err) if !retried => {
                    warn!("runtime sink request failed, restarting child: {}", err);
                    guard.restart().await?;
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
            config: self.config.clone(),
            binding: self.binding,
            filename,
            arrow_stream_bytes,
            cdc_ctx: cdc_ctx.cloned(),
        };
        self.send_sink_request(request).await
    }

    fn capability(&self) -> Option<&'static cdc::SinkCapability> {
        None
    }
}

pub struct RuntimeSchemaSinkPlugin {
    binding: RuntimeBinding,
    config: RuntimeSchemaConfig,
    connection: Mutex<RuntimeChildConnection>,
}

impl RuntimeSchemaSinkPlugin {
    pub async fn new(
        resolved: ResolvedRuntimePlugin,
        pipeline_name: String,
        binding: RuntimeBinding,
        config: RuntimeSchemaConfig,
    ) -> io::Result<Self> {
        let connection =
            RuntimeChildConnection::spawn(resolved.clone(), pipeline_name.clone()).await?;
        Ok(Self {
            binding,
            config,
            connection: Mutex::new(connection),
        })
    }

    async fn send_schema_request(&self, request: SchemaRunRequest) -> io::Result<()> {
        let mut retried = false;
        loop {
            let mut guard = self.connection.lock().await;
            if guard.has_exited()? {
                guard.restart().await?;
            }

            let send_result = guard.send(&HostFrame::RunSchema(request.clone())).await;
            let recv_result = match send_result {
                Ok(_) => guard.recv().await,
                Err(err) => Err(err),
            };

            match recv_result {
                Ok(PluginFrame::SchemaAck) => return Ok(()),
                Ok(PluginFrame::Error(err)) => return Err(io::Error::other(err)),
                Ok(other) => {
                    return Err(io::Error::other(format!(
                        "unexpected runtime schema frame: {:?}",
                        other
                    )))
                }
                Err(err) if !retried => {
                    warn!("runtime schema request failed, restarting child: {}", err);
                    guard.restart().await?;
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
        let request = SchemaRunRequest {
            config: self.config.clone(),
            binding: self.binding,
            namespace: namespace.to_string(),
            metadata: metadata.clone(),
        };
        self.send_schema_request(request).await
    }
}
