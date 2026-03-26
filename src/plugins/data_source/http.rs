use std::io::{Cursor, Read};
use std::sync::Arc;

use async_trait::async_trait;
use flate2::read::GzDecoder;
use reqwest::Client;
use serde_derive::Deserialize;

use crate::helpers::configuration::{Config, DataSourcePluginConfig};
use crate::helpers::offsets::{OffsetKey, OffsetTypes, Offsets};
use crate::ingest_work::{Ingest, IngestBatch, IngestTask, IngestTasks};
use crate::plugins::{DataSink, DataSource};

#[derive(Debug, Deserialize, Clone)]
pub struct DataSourceHttpPluginConfig {
    pub url: String,
    pub format: Option<String>,
    pub batch_size_bytes: Option<i64>,
    pub batch_size_seconds: Option<i64>,
}

impl From<DataSourcePluginConfig> for DataSourceHttpPluginConfig {
    fn from(plugin_config: DataSourcePluginConfig) -> Self {
        match plugin_config {
            DataSourcePluginConfig::Http(config) => config,
            _ => panic!("Invalid plugin type for HTTP"),
        }
    }
}

pub struct DataSourceHttpPlugin {
    client: Client,
    ingest: Ingest,
    config: DataSourceHttpPluginConfig,
}

impl DataSourceHttpPlugin {
    pub async fn new() -> Self {
        let config: DataSourceHttpPluginConfig =
            match Config::get_pipeline_input_plugin_config() {
                Ok(c) => c.into(),
                Err(_) => DataSourceHttpPluginConfig {
                    url: Config::getenv("DATA_SOURCE_HTTP_URL", ""),
                    format: None,
                    batch_size_bytes: Some(
                        Config::getenv("DATA_SOURCE_BATCH_SIZE_BYTES", "1024000")
                            .parse()
                            .unwrap_or(1_024_000),
                    ),
                    batch_size_seconds: Some(
                        Config::getenv("DATA_SOURCE_BATCH_SIZE_SECONDS", "600")
                            .parse()
                            .unwrap_or(600),
                    ),
                },
            };

        Self {
            client: Client::new(),
            ingest: Ingest::new(),
            config,
        }
    }
}

#[async_trait]
impl DataSource for DataSourceHttpPlugin {
    async fn sync(
        &mut self,
        offsets: Arc<Offsets>,
        shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
    ) -> Result<(), std::io::Error> {
        if self.config.url.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "http url is empty",
            ));
        }

        let offset_key = OffsetKey {
            namespace: self.config.url.clone(),
            partition: String::new(),
        };

        if offsets.validate(&offset_key, OffsetTypes::Closed, 1) == Some(true) {
            return Ok(());
        }

        let response = self
            .client
            .get(&self.config.url)
            .send()
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let response = response
            .error_for_status()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let body = response
            .bytes()
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let text = if self.config.url.contains(".gz") {
            let mut decoder = GzDecoder::new(Cursor::new(body.to_vec()));
            let mut decompressed = String::new();
            decoder
                .read_to_string(&mut decompressed)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            decompressed
        } else {
            String::from_utf8_lossy(&body).into_owned()
        };

        if text.is_empty() {
            return Ok(());
        }

        let chunk_bytes = self
            .config
            .batch_size_bytes
            .unwrap_or(
                Config::getenv("DATA_SOURCE_BATCH_SIZE_BYTES", "1024000")
                    .parse()
                    .unwrap_or(1_024_000),
            ) as usize;

        let source_uri = self.config.url.clone();
        let mut ingest_tasks = IngestTasks::new();
        let mut buf = String::new();
        let mut has_tasks = false;

        for line in text.lines() {
            if !buf.is_empty() {
                buf.push('\n');
            }
            buf.push_str(line);
            if buf.len() >= chunk_bytes {
                let data = std::mem::take(&mut buf);
                let bytes = data.len();
                ingest_tasks.add(IngestTask::new(
                    vec![IngestBatch {
                        offset_key: offset_key.clone(),
                        data,
                        bytes,
                        source_uri: source_uri.clone(),
                        namespace: Some("http".to_string()),
                    }],
                    offsets.clone(),
                    shared_output.clone(),
                ));
                has_tasks = true;
            }
        }

        if !buf.is_empty() {
            let bytes = buf.len();
            ingest_tasks.add(IngestTask::new(
                vec![IngestBatch {
                    offset_key: offset_key.clone(),
                    data: buf,
                    bytes,
                    source_uri: source_uri.clone(),
                    namespace: Some("http".to_string()),
                }],
                offsets.clone(),
                shared_output.clone(),
            ));
            has_tasks = true;
        }

        if has_tasks {
            self.ingest.ingest_file(
                &Arc::new(ingest_tasks),
                &offsets,
                shared_output,
            );
        }

        Ok(())
    }
}
