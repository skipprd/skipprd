use super::parquet_util::serialize_to_parquet;
use skippr_runtime_sdk::plugins::DataSink;
use async_trait::async_trait;
use datafusion::execution::SendableRecordBatchStream;
use serde_derive::Deserialize;
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

#[async_trait]
impl DataSink for DataSinkSftpPlugin {
    async fn sync(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
        cdc_ctx: Option<&skippr_runtime_sdk::plugins::cdc::SyncContext>,
    ) -> Result<(), std::io::Error> {
        let stream = match cdc_ctx {
            Some(ctx) => super::cdc_encode::augment_stream_with_cdc_columns(stream, &ctx.part_meta),
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

        let md5_digest = md5::compute(&filename);
        let remote_file_path = format!(
            "{}/{}.parquet",
            self.config.remote_path.trim_end_matches('/'),
            hex::encode(md5_digest.0)
        );

        let mut remote_file = sftp
            .create(std::path::Path::new(&remote_file_path))
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        remote_file.write_all(&parquet_bytes.bytes)?;

        info!("SFTP: uploaded to {}", remote_file_path);
        counters::dec_uploads_in_flight();
        Ok(())
    }

    fn capability(&self) -> Option<&'static skippr_runtime_sdk::plugins::cdc::SinkCapability> {
        Some(&skippr_runtime_sdk::plugins::cdc::sink_capabilities::SFTP)
    }
}

impl DataSinkSftpPlugin {
    pub async fn new_with_config(_buffer_name: String, config: DataSinkSftpPluginConfig) -> Self {
        Self { config }
    }
}
