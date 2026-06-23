use super::parquet_util::serialize_to_parquet;
use async_trait::async_trait;
use datafusion::execution::SendableRecordBatchStream;
use serde_derive::Deserialize;
use skippr_runtime_sdk::plugins::DataSink;
use ssh2::Session;
use std::io::Write;
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
        ctx: skippr_runtime_sdk::plugins::SinkWriteContext<'_>,
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

    fn capability(&self) -> &'static skippr_runtime_sdk::plugins::cdc::SinkCapability {
        &skippr_runtime_sdk::plugins::cdc::sink_capabilities::SFTP
    }
}

impl DataSinkSftpPlugin {
    pub async fn new_with_config(_buffer_name: String, config: DataSinkSftpPluginConfig) -> Self {
        Self { config }
    }
}
