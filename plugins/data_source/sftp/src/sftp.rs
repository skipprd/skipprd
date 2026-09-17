use std::io::Read as IoRead;
use std::net::TcpStream;
use std::sync::Arc;

use async_trait::async_trait;
use serde_derive::Deserialize;
use ssh2::Session;
use tracing::info;

use crate::helpers::plugin_config::PluginConfigEntry;
use skippr_runtime_sdk::plugins::DataSource;
use skippr_runtime_sdk::progress::OffsetKey;
use skippr_runtime_sdk::source_compat::{submit_payload_batches, IngestBatch, SourceSyncContext};

#[derive(Debug, Deserialize, Clone)]
pub struct DataSourceSftpPluginConfig {
    pub host: String,
    pub port: Option<u16>,
    pub username: String,
    pub password: Option<String>,
    pub private_key_path: Option<String>,
    pub remote_path: String,
    pub format: Option<String>,
    pub batch_size_bytes: Option<i64>,
    pub batch_size_seconds: Option<i64>,
}

impl TryFrom<PluginConfigEntry> for DataSourceSftpPluginConfig {
    type Error = String;

    fn try_from(plugin_config: PluginConfigEntry) -> Result<Self, Self::Error> {
        plugin_config.decode_for_plugin("Sftp")
    }
}

pub struct DataSourceSftpPlugin {
    config: DataSourceSftpPluginConfig,
}

impl DataSourceSftpPlugin {

    pub fn with_runtime_config(config: DataSourceSftpPluginConfig) -> Self {
        Self { config }
    }

    fn connect(&self) -> Result<Session, std::io::Error> {
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

        Ok(sess)
    }
}

#[async_trait]
impl DataSource for DataSourceSftpPlugin {
    async fn sync(&mut self, ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error> {
        let sess = self.connect()?;
        let sftp = sess
            .sftp()
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let remote_path = std::path::Path::new(&self.config.remote_path);
        let entries: Vec<String> = if self.config.remote_path.contains('*') {
            let parent = remote_path.parent().unwrap_or(std::path::Path::new("/"));
            let pattern = remote_path
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_default();
            let dir_entries = sftp
                .readdir(parent)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            dir_entries
                .into_iter()
                .filter(|(path, _)| {
                    let name = path
                        .file_name()
                        .map(|f| f.to_string_lossy().to_string())
                        .unwrap_or_default();
                    glob::Pattern::new(&pattern)
                        .map(|p| p.matches(&name))
                        .unwrap_or(false)
                })
                .map(|(path, _)| path.to_string_lossy().to_string())
                .collect()
        } else {
            vec![self.config.remote_path.clone()]
        };

        info!("SFTP: found {} file(s) to download", entries.len());

        for file_path in entries {
            let mut remote_file = sftp
                .open(std::path::Path::new(&file_path))
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            let mut contents = String::new();
            remote_file
                .read_to_string(&mut contents)
                .map_err(|e| std::io::Error::other(e.to_string()))?;

            if contents.is_empty() {
                continue;
            }

            let filename = std::path::Path::new(&file_path)
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_else(|| "file".to_string());
            let namespace = format!("sftp.{}", filename);
            let offset_key = OffsetKey {
                namespace: namespace.clone(),
                partition: file_path.clone(),
            };

            let bytes = contents.len();
            submit_payload_batches(
                ctx.as_ref(),
                vec![IngestBatch {
                    offset_key,
                    data: contents,
                    bytes,
                    offset_pos: None,
                    source_uri: format!("sftp://{}{}", self.config.host, file_path),
                    namespace: Some(namespace),
                    cdc_rows: None,
                }],
            )?;
        }

        Ok(())
    }

    fn execution_contract(&self) -> skippr_runtime_sdk::plugins::SourceExecutionContract {
        skippr_runtime_sdk::plugins::SourceExecutionContract::finite()
    }
}
