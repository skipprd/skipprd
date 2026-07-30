use super::parquet_util::serialize_to_parquet;
use async_trait::async_trait;
use datafusion::execution::SendableRecordBatchStream;
use serde_derive::Deserialize;
use skippr_runtime_sdk::plugins::DataSink;
use skippr_runtime_sdk::plugins::{SinkWriteContext, SinkWriteOutcome};
use skippr_runtime_sdk::sink_idempotency::{manifest_object_name, ObjectWriteManifest};
use ssh2::Session;
use std::io::{Read, Write};
use std::net::TcpStream;
use tracing::info;

use crate::helpers::configuration::DataSinkPluginConfig;

#[derive(Debug, Deserialize, Clone)]
pub struct DataSinkSftpPluginConfig {
    pub host: String,
    pub port: Option<u16>,
    pub username: String,
    pub password: Option<String>,
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
}

skippr_runtime_sdk::declare_sink_spec!(
    SftpSinkSpec,
    DataSinkSftpPlugin,
    skippr_runtime_sdk::plugins::cdc::sink_capabilities::SFTP,
    skippr_runtime_sdk::plugins::SftpAtomicRename
);

#[async_trait]
impl DataSink for DataSinkSftpPlugin {
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
        use skippr_runtime_sdk::metrics::counters;
        counters::inc_uploads_in_flight();

        let parquet_bytes = serialize_to_parquet(stream).await?;

        let port = self.config.port.unwrap_or(22);
        let tcp = TcpStream::connect(format!("{}:{}", self.config.host, port))?;
        let mut sess = Session::new().map_err(|e| std::io::Error::other(e.to_string()))?;
        sess.set_tcp_stream(tcp);
        sess.handshake()
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        if let Some(ref key_path) = self.config.private_key_path {
            sess.userauth_pubkey_file(
                &self.config.username,
                None,
                std::path::Path::new(key_path),
                None,
            )
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        } else if let Some(ref password) = self.config.password {
            sess.userauth_password(&self.config.username, password)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        }

        let sftp = sess
            .sftp()
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let object_stem = if ctx.idempotency_key.is_empty() {
            hex::encode(md5::compute(&ctx.filename).0)
        } else {
            skippr_runtime_sdk::sink_idempotency::deterministic_object_name(
                &ctx.idempotency_key,
                "",
            )?
        };
        let remote_file_path = format!(
            "{}/{}.parquet",
            self.config.remote_path.trim_end_matches('/'),
            object_stem
        );
        let remote_tmp_path = format!("{remote_file_path}.tmp");

        let mut remote_file = sftp
            .create(std::path::Path::new(&remote_tmp_path))
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        remote_file.write_all(&parquet_bytes.bytes)?;
        drop(remote_file);
        sftp.rename(
            std::path::Path::new(&remote_tmp_path),
            std::path::Path::new(&remote_file_path),
            None,
        )
        .map_err(|e| std::io::Error::other(e.to_string()))?;

        info!("SFTP: uploaded to {}", remote_file_path);
        counters::dec_uploads_in_flight();
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
        self.sync_with_context(stream, ctx).await?;
        self.write_remote_manifest(&remote_manifest_path, &expected_manifest)
            .await?;
        Ok(SinkWriteOutcome::Applied)
    }

    async fn sync_grouped(
        &self,
        mut reader: skippr_runtime_sdk::plugins::GroupedBatchReader,
        ctx: skippr_runtime_sdk::plugins::GroupedSinkWriteContext<'_>,
    ) -> Result<SinkWriteOutcome, std::io::Error> {
        let schema = reader.schema();
        let mut applied = false;
        while let Some(chunk) = reader.next_chunk().await? {
            let chunk_cdc = ctx.chunk_cdc_context(&chunk)?;
            let chunk_ctx = ctx.chunk_sink_write_context_with_cdc(
                chunk.chunk_index,
                chunk.chunk_index == 0 && chunk.final_chunk,
                chunk_cdc.as_ref(),
            );
            if self
                .sync_with_context_result(chunk.into_stream(schema.clone()), chunk_ctx)
                .await?
                == SinkWriteOutcome::Applied
            {
                applied = true;
            }
        }
        Ok(if applied {
            SinkWriteOutcome::Applied
        } else {
            SinkWriteOutcome::AlreadyApplied
        })
    }

    fn capability(&self) -> &'static skippr_runtime_sdk::plugins::cdc::SinkCapability {
        &skippr_runtime_sdk::plugins::cdc::sink_capabilities::SFTP
    }
}

impl DataSinkSftpPlugin {
    pub async fn new_with_config(_buffer_name: String, config: DataSinkSftpPluginConfig) -> Self {
        Self { config }
    }

    fn remote_file_path(&self, object_stem: &str) -> String {
        format!(
            "{}/{}.parquet",
            self.config.remote_path.trim_end_matches('/'),
            object_stem
        )
    }

    fn connect(&self) -> Result<ssh2::Sftp, std::io::Error> {
        let port = self.config.port.unwrap_or(22);
        let tcp = TcpStream::connect(format!("{}:{}", self.config.host, port))?;
        let mut sess = Session::new().map_err(|e| std::io::Error::other(e.to_string()))?;
        sess.set_tcp_stream(tcp);
        sess.handshake()
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        if let Some(ref key_path) = self.config.private_key_path {
            sess.userauth_pubkey_file(
                &self.config.username,
                None,
                std::path::Path::new(key_path),
                None,
            )
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        } else if let Some(ref password) = self.config.password {
            sess.userauth_password(&self.config.username, password)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        }

        sess.sftp()
            .map_err(|e| std::io::Error::other(e.to_string()))
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
        let manifest = ObjectWriteManifest::from_json_bytes(&bytes)?;
        Ok(manifest.matches_manifest(expected))
    }

    async fn write_remote_manifest(
        &self,
        path: &str,
        manifest: &ObjectWriteManifest,
    ) -> Result<(), std::io::Error> {
        let sftp = self.connect()?;
        let mut file = sftp
            .create(std::path::Path::new(path))
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        file.write_all(&manifest.to_json_bytes()?)?;
        Ok(())
    }
}
