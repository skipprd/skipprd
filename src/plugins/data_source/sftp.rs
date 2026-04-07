use std::io::Read as IoRead;
use std::net::TcpStream;
use std::sync::Arc;

use async_trait::async_trait;
use serde_derive::Deserialize;
use ssh2::Session;
use tracing::info;

use crate::helpers::configuration::{Config, DataSourcePluginConfig};
use crate::helpers::offsets::{OffsetKey, Offsets};
use crate::ingest_work::{Ingest, IngestBatch, IngestTask, IngestTasks};
use crate::plugins::{DataSink, DataSource};

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

impl From<DataSourcePluginConfig> for DataSourceSftpPluginConfig {
    fn from(plugin_config: DataSourcePluginConfig) -> Self {
        match plugin_config {
            DataSourcePluginConfig::Sftp(config) => config,
            _ => panic!("Invalid plugin type for SFTP"),
        }
    }
}

pub struct DataSourceSftpPlugin {
    ingest: Ingest,
    config: DataSourceSftpPluginConfig,
}

impl DataSourceSftpPlugin {
    pub async fn new() -> Self {
        let config: DataSourceSftpPluginConfig = match Config::get_pipeline_input_plugin_config() {
            Ok(c) => c.into(),
            Err(_) => DataSourceSftpPluginConfig {
                host: Config::getenv("SFTP_HOST", ""),
                port: None,
                username: Config::getenv("SFTP_USERNAME", ""),
                password: Some(Config::getenv("SFTP_PASSWORD", "")),
                private_key_path: None,
                remote_path: Config::getenv("SFTP_REMOTE_PATH", ""),
                format: None,
                batch_size_bytes: None,
                batch_size_seconds: None,
            },
        };
        Self {
            ingest: Ingest::new(),
            config,
        }
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
    async fn sync(
        &mut self,
        offsets: Arc<Offsets>,
        shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
    ) -> Result<(), std::io::Error> {
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
            let mut ingest_tasks = IngestTasks::new();
            ingest_tasks.add(IngestTask::new(
                vec![IngestBatch {
                    offset_key,
                    data: contents,
                    bytes,
                    source_uri: format!("sftp://{}{}", self.config.host, file_path),
                    namespace: Some(namespace),
                    cdc_rows: None,
                }],
                offsets.clone(),
                shared_output.clone(),
            ));
            self.ingest
                .ingest_file(&Arc::new(ingest_tasks), &offsets, shared_output.clone());
        }

        Ok(())
    }
}
