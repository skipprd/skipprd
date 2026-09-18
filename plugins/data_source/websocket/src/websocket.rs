use skippr_runtime_sdk::SkipprConfig;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use serde_derive::Deserialize;
use tokio::time::{Duration, Instant};
use tokio_tungstenite::connect_async;
use tracing::{error, info};

use crate::helpers::plugin_config::PluginConfigEntry;
use crate::RUNNING;
use skippr_runtime_sdk::plugins::{DataSource, SourceExecutionContract, SourceOnceContract};
use skippr_runtime_sdk::progress::OffsetKey;
use skippr_runtime_sdk::source_compat::{submit_payload_batches, IngestBatch, SourceSyncContext};

#[derive(Debug, Deserialize, SkipprConfig, Clone)]
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
    config: DataSourceWebsocketPluginConfig,
}

impl DataSourceWebsocketPlugin {

    pub fn with_runtime_config(config: DataSourceWebsocketPluginConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl DataSource for DataSourceWebsocketPlugin {
    async fn sync(&mut self, ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error> {
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
                    submit_payload_batches(
                        ctx.as_ref(),
                        vec![IngestBatch {
                            offset_key,
                            data,
                            bytes,
                            offset_pos: None,
                            source_uri: self.config.url.clone(),
                            namespace: Some(namespace.clone()),
                            cdc_rows: None,
                        }],
                    )?;
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

    fn execution_contract(&self) -> SourceExecutionContract {
        if self.config.mode.as_deref() == Some("batch") {
            SourceExecutionContract::stream(SourceOnceContract::PluginIdleBounded)
        } else {
            SourceExecutionContract::stream(SourceOnceContract::HostIdleBounded)
        }
    }
}
