use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use serde_derive::Deserialize;
use tokio::time::{Duration, Instant};
use tokio_tungstenite::connect_async;
use tracing::{error, info};

use crate::helpers::configuration::Config;
use crate::helpers::offsets::{OffsetKey, Offsets};
use crate::helpers::plugin_config::PluginConfigEntry;
use crate::ingest_work::{Ingest, IngestBatch, IngestTask, IngestTasks};
use crate::plugins::{DataSink, DataSource};
use crate::RUNNING;

#[derive(Debug, Deserialize, Clone)]
pub struct DataSourceWebsocketPluginConfig {
    pub url: String,
    pub headers: Option<HashMap<String, String>>,
    pub ping_interval_seconds: Option<u64>,
    pub mode: Option<String>,
    pub idle_timeout_seconds: Option<u64>,
    pub format: Option<String>,
    pub batch_size_bytes: Option<i64>,
    pub batch_size_seconds: Option<i64>,
}

impl TryFrom<PluginConfigEntry> for DataSourceWebsocketPluginConfig {
    type Error = String;

    fn try_from(plugin_config: PluginConfigEntry) -> Result<Self, Self::Error> {
        plugin_config.decode_for_plugin("Websocket")
    }
}

pub struct DataSourceWebsocketPlugin {
    ingest: Ingest,
    config: DataSourceWebsocketPluginConfig,
}

impl DataSourceWebsocketPlugin {
    pub async fn new() -> Self {
        let config: DataSourceWebsocketPluginConfig =
            match Config::get_pipeline_input_plugin_config() {
                Ok(c) => c.try_into().unwrap_or_else(|e| panic!("{}", e)),
                Err(_) => DataSourceWebsocketPluginConfig {
                    url: Config::getenv("WEBSOCKET_URL", ""),
                    headers: None,
                    ping_interval_seconds: None,
                    mode: None,
                    idle_timeout_seconds: None,
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

    pub fn with_runtime_config(config: DataSourceWebsocketPluginConfig) -> Self {
        Self {
            ingest: Ingest::new(),
            config,
        }
    }
}

#[async_trait]
impl DataSource for DataSourceWebsocketPlugin {
    async fn sync(
        &mut self,
        offsets: Arc<Offsets>,
        shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
    ) -> Result<(), std::io::Error> {
        let (ws_stream, _) = connect_async(&self.config.url)
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let (_write, mut read) = ws_stream.split();

        let namespace = format!(
            "websocket.{}",
            url::Url::parse(&self.config.url)
                .map(|u| u.host_str().unwrap_or("unknown").to_string())
                .unwrap_or_else(|_| "unknown".to_string())
        );
        let is_batch = self.config.mode.as_deref() == Some("batch");
        let idle_timeout_dur = Duration::from_secs(self.config.idle_timeout_seconds.unwrap_or(5));
        let mut last_msg = Instant::now();
        let mut counter: u64 = 0;

        info!("WebSocket: connected to {}", self.config.url);

        while RUNNING.read().load(Ordering::SeqCst) {
            match tokio::time::timeout(Duration::from_secs(1), read.next()).await {
                Ok(Some(Ok(msg))) => {
                    let data = match msg {
                        tungstenite::Message::Text(t) => t,
                        tungstenite::Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
                        tungstenite::Message::Ping(_) | tungstenite::Message::Pong(_) => continue,
                        tungstenite::Message::Close(_) => break,
                        _ => continue,
                    };
                    if data.is_empty() {
                        continue;
                    }
                    last_msg = Instant::now();
                    counter += 1;
                    let bytes = data.len();
                    let offset_key = OffsetKey {
                        namespace: namespace.clone(),
                        partition: counter.to_string(),
                    };
                    let mut ingest_tasks = IngestTasks::new();
                    ingest_tasks.add(IngestTask::new(
                        vec![IngestBatch {
                            offset_key,
                            data,
                            bytes,
                            source_uri: self.config.url.clone(),
                            namespace: Some(namespace.clone()),
                            cdc_rows: None,
                        }],
                        offsets.clone(),
                        shared_output.clone(),
                    ));
                    self.ingest.ingest_file(
                        &Arc::new(ingest_tasks),
                        &offsets,
                        shared_output.clone(),
                    );
                }
                Ok(Some(Err(e))) => {
                    error!("WebSocket error: {}", e);
                    break;
                }
                Ok(None) => break,
                Err(_) => {
                    if is_batch && last_msg.elapsed() > idle_timeout_dur {
                        info!("WebSocket batch mode: idle timeout reached, exiting");
                        break;
                    }
                }
            }
        }

        Ok(())
    }
}
