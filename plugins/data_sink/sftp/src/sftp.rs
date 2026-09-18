use skippr_runtime_sdk::SkipprConfig;
use async_trait::async_trait;
use datafusion::execution::SendableRecordBatchStream;
use futures::StreamExt;
use serde_derive::Deserialize;
use skippr_object_writer::backends::AtomicFileBackend;
use skippr_object_writer::{
    CompletionMetadata, MultipartUpload, ObjectPartReceipt, ObjectWriteBackend, ObjectWriteError,
    ObjectWriteReceipt, ObjectWriteRequest, ObjectWriteSession, ObjectWriterConfig, PartMetadata,
};
use skippr_runtime_sdk::plugins::DataSink;
use skippr_runtime_sdk::plugins::{SinkPreflightOutcome, SinkWriteContext, SinkWriteOutcome};
use skippr_runtime_sdk::sink_idempotency::{
    legacy_chunk_idempotency_key, manifest_object_name, persisted_object_write_matches,
    GroupedWriteReceipt, ObjectWriteManifest,
};
use ssh2::Session;
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::info;

use crate::helpers::configuration::DataSinkPluginConfig;

#[derive(Debug, Deserialize, SkipprConfig, Clone)]
pub struct DataSinkSftpPluginConfig {
    pub host: String,
    pub port: Option<u16>,
    pub username: String,
    #[skippr(secret)]
    pub password: Option<String>,
    #[skippr(secret_path)]
    pub private_key_path: Option<String>,
    pub remote_path: String,
    pub format: Option<String>,
}

impl TryFrom<DataSinkPluginConfig> for DataSinkSftpPluginConfig {
    type Error = String;

    fn try_from(entry: DataSinkPluginConfig) -> Result<Self, Self::Error> {
        entry.decode_for_plugin("Sftp")
    }
}

pub struct DataSinkSftpPlugin {
    config: DataSinkSftpPluginConfig,
    object_backend: Arc<SftpObjectBackend>,
}

#[derive(Clone)]
struct SftpUpload {
    spool_upload: MultipartUpload,
    spool_path: PathBuf,
    remote_file_path: String,
    remote_tmp_path: String,
}

/// SFTP has no multipart commit API, so each bounded compaction envelope is
/// spooled under `${TMPDIR}/skippr-object-writer/sftp`, then copied with
/// constant memory to the remote `.tmp` target and atomically renamed.
struct SftpObjectBackend {
    config: DataSinkSftpPluginConfig,
    spool_backend: Arc<AtomicFileBackend>,
    spool_root: PathBuf,
    uploads: Mutex<HashMap<String, SftpUpload>>,
    next_upload_id: AtomicU64,
}

fn connect_with_config(config: &DataSinkSftpPluginConfig) -> Result<ssh2::Sftp, std::io::Error> {
    let port = config.port.unwrap_or(22);
    let tcp = TcpStream::connect(format!("{}:{}", config.host, port))?;
    let mut session = Session::new().map_err(|error| std::io::Error::other(error.to_string()))?;
    session.set_tcp_stream(tcp);
    session
        .handshake()
        .map_err(|error| std::io::Error::other(error.to_string()))?;

    if let Some(ref key_path) = config.private_key_path {
        session
            .userauth_pubkey_file(&config.username, None, std::path::Path::new(key_path), None)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
    } else if let Some(ref password) = config.password {
        session
            .userauth_password(&config.username, password)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
    }
    session
        .sftp()
        .map_err(|error| std::io::Error::other(error.to_string()))
}

#[async_trait]
impl ObjectWriteBackend for SftpObjectBackend {
    type Error = std::io::Error;

    async fn begin(&self, request: &ObjectWriteRequest) -> Result<MultipartUpload, Self::Error> {
        tokio::fs::create_dir_all(&self.spool_root).await?;
        let sequence = self.next_upload_id.fetch_add(1, Ordering::Relaxed);
        let upload_id = format!("sftp-{sequence}");
        let spool_path = self.spool_root.join(format!("{upload_id}.parquet"));
        let spool_upload = self
            .spool_backend
            .begin(&ObjectWriteRequest::new(spool_path.to_string_lossy()))
            .await
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let remote_file_path = request.object_key.clone();
        let remote_tmp_path = format!("{remote_file_path}.tmp");
        self.uploads.lock().await.insert(
            upload_id.clone(),
            SftpUpload {
                spool_upload,
                spool_path: spool_path.clone(),
                remote_file_path,
                remote_tmp_path: remote_tmp_path.clone(),
            },
        );
        Ok(MultipartUpload {
            upload_id,
            metadata: BTreeMap::from([
                (
                    "spool_path".to_string(),
                    spool_path.to_string_lossy().into_owned(),
                ),
                ("remote_tmp_path".to_string(), remote_tmp_path),
            ]),
        })
    }

    async fn upload_part(
        &self,
        upload: &MultipartUpload,
        part_number: u32,
        bytes: bytes::Bytes,
    ) -> Result<PartMetadata, Self::Error> {
        let state = self
            .uploads
            .lock()
            .await
            .get(&upload.upload_id)
            .cloned()
            .ok_or_else(|| std::io::Error::other("unknown SFTP upload"))?;
        self.spool_backend
            .upload_part(&state.spool_upload, part_number, bytes)
            .await
            .map_err(|error| std::io::Error::other(error.to_string()))
    }

    async fn complete(
        &self,
        upload: &MultipartUpload,
        parts: &[ObjectPartReceipt],
    ) -> Result<CompletionMetadata, Self::Error> {
        let state = self
            .uploads
            .lock()
            .await
            .get(&upload.upload_id)
            .cloned()
            .ok_or_else(|| std::io::Error::other("unknown SFTP upload"))?;
        self.spool_backend
            .complete(&state.spool_upload, parts)
            .await
            .map_err(|error| std::io::Error::other(error.to_string()))?;

        let config = self.config.clone();
        let spool_path = state.spool_path.clone();
        let remote_tmp_path = state.remote_tmp_path.clone();
        let remote_file_path = state.remote_file_path.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            let sftp = connect_with_config(&config)?;
            let mut source = std::fs::File::open(&spool_path)?;
            let mut remote = sftp
                .create(std::path::Path::new(&remote_tmp_path))
                .map_err(|error| std::io::Error::other(error.to_string()))?;
            std::io::copy(&mut source, &mut remote)?;
            remote.flush()?;
            drop(remote);
            sftp.rename(
                std::path::Path::new(&remote_tmp_path),
                std::path::Path::new(&remote_file_path),
                None,
            )
            .map_err(|error| std::io::Error::other(error.to_string()))?;
            Ok(())
        })
        .await
        .map_err(|error| std::io::Error::other(error.to_string()))??;

        let _ = tokio::fs::remove_file(&state.spool_path).await;
        self.uploads.lock().await.remove(&upload.upload_id);
        let bytes = parts.iter().map(|part| part.bytes).sum::<u64>();
        Ok(CompletionMetadata {
            etag: Some(format!("sftp-{bytes}")),
            metadata: BTreeMap::from([("remote_path".to_string(), state.remote_file_path)]),
            ..Default::default()
        })
    }

    async fn abort(&self, upload: &MultipartUpload) -> Result<(), Self::Error> {
        let Some(state) = self.uploads.lock().await.remove(&upload.upload_id) else {
            return Ok(());
        };
        let _ = self.spool_backend.abort(&state.spool_upload).await;
        let _ = tokio::fs::remove_file(&state.spool_path).await;

        let config = self.config.clone();
        let remote_tmp_path = state.remote_tmp_path;
        let _ = tokio::task::spawn_blocking(move || {
            let sftp = connect_with_config(&config)?;
            match sftp.unlink(std::path::Path::new(&remote_tmp_path)) {
                Ok(()) => Ok(()),
                Err(error) if error.code() == ssh2::ErrorCode::SFTP(2) => Ok(()),
                Err(error) => Err(std::io::Error::other(error.to_string())),
            }
        })
        .await;
        Ok(())
    }
}

skippr_runtime_sdk::declare_sink_spec!(
    SftpSinkSpec,
    DataSinkSftpPlugin,
    skippr_runtime_sdk::plugins::cdc::sink_capabilities::SFTP,
    skippr_runtime_sdk::plugins::SftpAtomicRename
);

#[async_trait]
impl DataSink for DataSinkSftpPlugin {
    async fn preflight(
        &self,
        ctx: SinkWriteContext<'_>,
    ) -> Result<SinkPreflightOutcome, std::io::Error> {
        if !ctx.is_grouped() {
            return Ok(SinkPreflightOutcome::Ready);
        }
        ctx.validate_grouped::<skippr_runtime_sdk::plugins::SftpAtomicRename>()
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::Unsupported, err))?;
        let object_stem = skippr_runtime_sdk::sink_idempotency::deterministic_object_name(
            &ctx.idempotency_key,
            "",
        )?;
        let remote_file_path = self.remote_file_path(&object_stem);
        let remote_manifest_path = format!(
            "{}/{}",
            self.config.remote_path.trim_end_matches('/'),
            manifest_object_name(
                remote_file_path
                    .rsplit_once('/')
                    .map(|(_, name)| name)
                    .unwrap_or("output.parquet")
            )
        );
        let expected_manifest = ObjectWriteManifest::from_context(
            ctx.compaction_id,
            ctx.idempotency_key,
            ctx.schema_fingerprint,
            &ctx.wal_refs,
        );
        if self
            .remote_manifest_matches(&remote_manifest_path, &expected_manifest)
            .await?
        {
            return Ok(SinkPreflightOutcome::AlreadyApplied {
                authority: format!(
                    "sftp://{}@{}{}",
                    self.config.username, self.config.host, remote_manifest_path
                ),
            });
        }
        Ok(SinkPreflightOutcome::Ready)
    }

    async fn sync(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
        cdc_ctx: Option<&skippr_runtime_sdk::plugins::cdc::SyncContext>,
    ) -> Result<(), std::io::Error> {
        self.sync_with_context(
            stream,
            skippr_runtime_sdk::plugins::SinkWriteContext {
                filename,
                compaction_id: String::new(),
                idempotency_key: String::new(),
                wal_refs: Vec::new(),
                write_semantics: skippr_runtime_sdk::plugins::SinkWriteSemantics::AtLeastOnce,
                schema_fingerprint: String::new(),
                cdc_ctx,
                source_contract: None,
            },
        )
        .await
    }

    async fn sync_with_context(
        &self,
        stream: SendableRecordBatchStream,
        ctx: SinkWriteContext<'_>,
    ) -> Result<(), std::io::Error> {
        ctx.validate_grouped::<skippr_runtime_sdk::plugins::SftpAtomicRename>()
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::Unsupported, err))?;
        let stream = match ctx.cdc_ctx {
            Some(cdc) => super::cdc_encode::augment_stream_with_cdc_columns(stream, &cdc.part_meta),
            None => stream,
        };
        let object_stem = if ctx.idempotency_key.is_empty() {
            hex::encode(md5::compute(&ctx.filename).0)
        } else {
            skippr_runtime_sdk::sink_idempotency::deterministic_object_name(
                &ctx.idempotency_key,
                "",
            )?
        };
        let remote_file_path = self.remote_file_path(&object_stem);
        self.write_stream(stream, &remote_file_path).await?;

        info!("SFTP: uploaded to {}", remote_file_path);
        Ok(())
    }

    async fn sync_with_context_result(
        &self,
        stream: SendableRecordBatchStream,
        ctx: SinkWriteContext<'_>,
    ) -> Result<SinkWriteOutcome, std::io::Error> {
        if !ctx.is_grouped() {
            self.sync_with_context(stream, ctx).await?;
            return Ok(SinkWriteOutcome::Applied);
        }
        ctx.validate_grouped::<skippr_runtime_sdk::plugins::SftpAtomicRename>()
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::Unsupported, err))?;
        let object_stem = skippr_runtime_sdk::sink_idempotency::deterministic_object_name(
            &ctx.idempotency_key,
            "",
        )?;
        let remote_file_path = self.remote_file_path(&object_stem);
        let remote_manifest_path = format!(
            "{}/{}",
            self.config.remote_path.trim_end_matches('/'),
            manifest_object_name(
                remote_file_path
                    .rsplit_once('/')
                    .map(|(_, name)| name)
                    .unwrap_or("output.parquet")
            )
        );
        let expected_manifest = ObjectWriteManifest::from_context(
            ctx.compaction_id.clone(),
            ctx.idempotency_key.clone(),
            ctx.schema_fingerprint.clone(),
            &ctx.wal_refs,
        );
        if self
            .remote_manifest_matches(&remote_manifest_path, &expected_manifest)
            .await?
        {
            return Ok(SinkWriteOutcome::AlreadyApplied);
        }
        let stream = match ctx.cdc_ctx {
            Some(cdc) => super::cdc_encode::augment_stream_with_cdc_columns(stream, &cdc.part_meta),
            None => stream,
        };
        let receipt = self.write_stream(stream, &remote_file_path).await?;
        self.write_remote_receipt(&remote_manifest_path, &expected_manifest, &receipt)
            .await?;
        Ok(SinkWriteOutcome::Applied)
    }

    async fn sync_grouped(
        &self,
        mut reader: skippr_runtime_sdk::plugins::GroupedBatchReader,
        ctx: skippr_runtime_sdk::plugins::GroupedSinkWriteContext<'_>,
    ) -> Result<SinkWriteOutcome, std::io::Error> {
        let object_stem = skippr_runtime_sdk::sink_idempotency::deterministic_object_name(
            &ctx.idempotency_key,
            "",
        )?;
        let remote_file_path = self.remote_file_path(&object_stem);
        let remote_manifest_path = format!(
            "{}/{}",
            self.config.remote_path.trim_end_matches('/'),
            manifest_object_name(
                remote_file_path
                    .rsplit_once('/')
                    .map(|(_, name)| name)
                    .unwrap_or("output.parquet")
            )
        );
        let expected_manifest = ObjectWriteManifest::from_context(
            ctx.compaction_id.clone(),
            ctx.idempotency_key.clone(),
            ctx.schema_fingerprint.clone(),
            ctx.wal_refs.as_slice(),
        );
        if self
            .remote_manifest_matches(&remote_manifest_path, &expected_manifest)
            .await?
        {
            return Ok(SinkWriteOutcome::AlreadyApplied);
        }
        if self.legacy_chunk_state_exists(&ctx, 0).await? {
            let receipt = self.sync_grouped_legacy_chunks(&mut reader, &ctx).await?;
            self.write_remote_receipt(&remote_manifest_path, &expected_manifest, &receipt)
                .await?;
            return Ok(SinkWriteOutcome::Applied);
        }
        let (stream, _stream_progress) = reader.into_stream();
        let stream = match ctx.cdc_ctx {
            Some(cdc) => super::cdc_encode::augment_stream_with_cdc_columns(stream, &cdc.part_meta),
            None => stream,
        };
        let receipt = self.write_stream(stream, &remote_file_path).await?;
        self.write_remote_receipt(&remote_manifest_path, &expected_manifest, &receipt)
            .await?;
        Ok(SinkWriteOutcome::Applied)
    }

    fn capability(&self) -> &'static skippr_runtime_sdk::plugins::cdc::SinkCapability {
        &skippr_runtime_sdk::plugins::cdc::sink_capabilities::SFTP
    }
}

impl DataSinkSftpPlugin {
    pub async fn new_with_config(_buffer_name: String, config: DataSinkSftpPluginConfig) -> Self {
        let object_backend = Arc::new(SftpObjectBackend {
            config: config.clone(),
            spool_backend: Arc::new(AtomicFileBackend::new()),
            spool_root: std::env::temp_dir().join("skippr-object-writer/sftp"),
            uploads: Mutex::new(HashMap::new()),
            next_upload_id: AtomicU64::new(1),
        });
        Self {
            config,
            object_backend,
        }
    }

    fn remote_file_path(&self, object_stem: &str) -> String {
        format!(
            "{}/{}.parquet",
            self.config.remote_path.trim_end_matches('/'),
            object_stem
        )
    }

    fn legacy_chunk_manifest(
        &self,
        ctx: &skippr_runtime_sdk::plugins::GroupedSinkWriteContext<'_>,
        chunk_index: u64,
    ) -> (String, String, ObjectWriteManifest) {
        let object_stem = legacy_chunk_idempotency_key(&ctx.idempotency_key, chunk_index);
        let remote_file_path = self.remote_file_path(&object_stem);
        let remote_manifest_path = format!(
            "{}/{}",
            self.config.remote_path.trim_end_matches('/'),
            manifest_object_name(
                remote_file_path
                    .rsplit_once('/')
                    .map(|(_, name)| name)
                    .unwrap_or("output.parquet")
            )
        );
        let manifest = ObjectWriteManifest::from_context(
            legacy_chunk_idempotency_key(&ctx.compaction_id, chunk_index),
            object_stem,
            ctx.schema_fingerprint.clone(),
            ctx.wal_refs.as_slice(),
        );
        (remote_file_path, remote_manifest_path, manifest)
    }

    async fn legacy_chunk_state_exists(
        &self,
        ctx: &skippr_runtime_sdk::plugins::GroupedSinkWriteContext<'_>,
        chunk_index: u64,
    ) -> std::io::Result<bool> {
        let (remote_file_path, path, expected) = self.legacy_chunk_manifest(ctx, chunk_index);
        if self.remote_manifest_matches(&path, &expected).await? {
            return Ok(true);
        }
        match self
            .connect()?
            .stat(std::path::Path::new(&remote_file_path))
        {
            Ok(_) => Ok(true),
            Err(error) if error.code() == ssh2::ErrorCode::SFTP(2) => Ok(false),
            Err(error) => Err(std::io::Error::other(error.to_string())),
        }
    }

    async fn sync_grouped_legacy_chunks(
        &self,
        reader: &mut skippr_runtime_sdk::plugins::GroupedBatchReader,
        ctx: &skippr_runtime_sdk::plugins::GroupedSinkWriteContext<'_>,
    ) -> std::io::Result<ObjectWriteReceipt> {
        let schema = reader.schema();
        let mut object_key = None;
        let mut rows = 0_u64;
        let mut bytes = 0_u64;
        let mut transport_chunk_count = 0_u32;
        let mut etag = None;

        while let Some(chunk) = reader.next_chunk().await? {
            let (remote_file_path, remote_manifest_path, expected) =
                self.legacy_chunk_manifest(ctx, chunk.chunk_index);
            let chunk_rows = chunk.rows;
            let (chunk_bytes, chunk_transport_count, chunk_etag) = if self
                .remote_manifest_matches(&remote_manifest_path, &expected)
                .await?
            {
                let stat = self
                    .connect()?
                    .stat(std::path::Path::new(&remote_file_path))
                    .map_err(|error| std::io::Error::other(error.to_string()))?;
                let size = stat.size.unwrap_or_default();
                (size, 1, Some(format!("sftp-{size}")))
            } else {
                let chunk_cdc = ctx.chunk_cdc_context(&chunk)?;
                let stream = chunk.into_stream(schema.clone());
                let stream = match chunk_cdc.as_ref() {
                    Some(cdc) => {
                        super::cdc_encode::augment_stream_with_cdc_columns(stream, &cdc.part_meta)
                    }
                    None => stream,
                };
                let receipt = self.write_stream(stream, &remote_file_path).await?;
                (receipt.bytes, receipt.transport_chunk_count, receipt.etag)
            };
            object_key.get_or_insert(remote_file_path);
            rows = rows
                .checked_add(chunk_rows)
                .ok_or_else(|| std::io::Error::other("legacy grouped row count overflow"))?;
            bytes = bytes
                .checked_add(chunk_bytes)
                .ok_or_else(|| std::io::Error::other("legacy grouped byte count overflow"))?;
            transport_chunk_count = transport_chunk_count
                .checked_add(chunk_transport_count)
                .ok_or_else(|| std::io::Error::other("legacy grouped chunk count overflow"))?;
            etag = chunk_etag;
        }

        Ok(ObjectWriteReceipt {
            version: 1,
            object_key: object_key
                .ok_or_else(|| std::io::Error::other("No rows to write to parquet"))?,
            upload_id: "legacy-chunk-replay".to_string(),
            rows,
            bytes,
            transport_chunk_count,
            parts: Vec::new(),
            etag,
            checksum: None,
            version_id: None,
            backend_metadata: BTreeMap::from([(
                "compatibility".to_string(),
                "legacy-read-only-chunks".to_string(),
            )]),
        })
    }

    fn connect(&self) -> Result<ssh2::Sftp, std::io::Error> {
        connect_with_config(&self.config)
    }

    async fn write_stream(
        &self,
        stream: SendableRecordBatchStream,
        remote_file_path: &str,
    ) -> std::io::Result<ObjectWriteReceipt> {
        use skippr_runtime_sdk::metrics::counters;

        let schema = stream.schema();
        let order_fields =
            skippr_runtime_sdk::converters::parquet_ordering::resolve_effective_order(&schema);
        let writer_properties =
            skippr_runtime_sdk::converters::parquet_ordering::build_writer_properties(
                &schema,
                &order_fields,
                skippr_runtime_sdk::converters::parquet_ordering::default_streaming_row_group_size(
                ),
            );
        let batches = stream.map(move |batch| {
            let batch = batch.map_err(ObjectWriteError::input)?;
            skippr_runtime_sdk::converters::parquet_ordering::sort_batch(&batch, &order_fields)
                .map_err(ObjectWriteError::input)
        });
        let session = ObjectWriteSession::new(
            self.object_backend.clone(),
            ObjectWriteRequest::new(remote_file_path),
            ObjectWriterConfig::default(),
        )
        .map_err(|error| std::io::Error::other(error.to_string()))?;

        counters::inc_uploads_in_flight();
        let result = session
            .write_parquet(schema, writer_properties, batches)
            .await;
        counters::dec_uploads_in_flight();
        let receipt = result.map_err(|error| std::io::Error::other(error.to_string()))?;
        counters::add_parquet_rows(receipt.rows);
        counters::add_parquet_bytes(receipt.bytes);
        counters::add_upload(1);
        Ok(receipt)
    }

    async fn remote_manifest_matches(
        &self,
        path: &str,
        expected: &ObjectWriteManifest,
    ) -> Result<bool, std::io::Error> {
        let sftp = self.connect()?;
        let mut file = match sftp.open(std::path::Path::new(path)) {
            Ok(file) => file,
            Err(err) => {
                let err = err.to_string();
                if err.contains("No such file") || err.contains("not found") {
                    return Ok(false);
                }
                return Err(std::io::Error::other(err));
            }
        };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        persisted_object_write_matches(&bytes, expected)
    }

    async fn write_remote_receipt(
        &self,
        path: &str,
        manifest: &ObjectWriteManifest,
        object_receipt: &ObjectWriteReceipt,
    ) -> Result<(), std::io::Error> {
        let receipt = GroupedWriteReceipt::from_manifest_and_upload(
            manifest,
            format!("sftp://{}{}", self.config.host, object_receipt.object_key),
            object_receipt.etag.clone().unwrap_or_default(),
            object_receipt.checksum.clone(),
            object_receipt.rows,
            object_receipt.bytes,
            object_receipt.transport_chunk_count,
        );
        let sftp = self.connect()?;
        let temporary_path = format!("{path}.tmp");
        let mut file = sftp
            .create(std::path::Path::new(&temporary_path))
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        file.write_all(&receipt.to_json_bytes()?)?;
        file.flush()?;
        drop(file);
        sftp.rename(
            std::path::Path::new(&temporary_path),
            std::path::Path::new(path),
            None,
        )
        .map_err(|error| std::io::Error::other(error.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> DataSinkSftpPluginConfig {
        DataSinkSftpPluginConfig {
            host: "example.test".to_string(),
            port: None,
            username: "user".to_string(),
            password: None,
            private_key_path: None,
            remote_path: "/exports/root/".to_string(),
            format: None,
        }
    }

    #[test]
    fn deterministic_remote_path_and_bounded_spool_location_are_stable() {
        let config = test_config();
        let spool_root = std::env::temp_dir().join("skippr-object-writer/sftp");
        let plugin = DataSinkSftpPlugin {
            config: config.clone(),
            object_backend: Arc::new(SftpObjectBackend {
                config,
                spool_backend: Arc::new(AtomicFileBackend::new()),
                spool_root: spool_root.clone(),
                uploads: Mutex::new(HashMap::new()),
                next_upload_id: AtomicU64::new(1),
            }),
        };

        assert_eq!(
            plugin.remote_file_path("apply-0001"),
            "/exports/root/apply-0001.parquet"
        );
        assert!(plugin
            .object_backend
            .spool_root
            .ends_with("skippr-object-writer/sftp"));
    }
}
