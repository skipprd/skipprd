use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;
use serde_derive::Deserialize;
use tokio::net::UdpSocket;
use tokio::time::Duration;
use tracing::{error, info};

use crate::helpers::configuration::Config;
use crate::helpers::plugin_config::PluginConfigEntry;
use crate::RUNNING;
use skippr_runtime_sdk::plugins::{DataSink, DataSource};
use skippr_runtime_sdk::progress::{OffsetKey, Offsets};
use skippr_runtime_sdk::source_compat::{Ingest, IngestBatch, IngestTask, IngestTasks};

#[derive(Debug, Deserialize, Clone)]
pub struct DataSourceStatsdPluginConfig {
    pub listen_address: Option<String>,
    pub format: Option<String>,
    pub batch_size_bytes: Option<i64>,
    pub batch_size_seconds: Option<i64>,
}

impl TryFrom<PluginConfigEntry> for DataSourceStatsdPluginConfig {
    type Error = String;

    fn try_from(plugin_config: PluginConfigEntry) -> Result<Self, Self::Error> {
        plugin_config.decode_for_plugin("Statsd")
    }
}

pub struct DataSourceStatsdPlugin {
    ingest: Ingest,
    config: DataSourceStatsdPluginConfig,
}

impl DataSourceStatsdPlugin {
    pub async fn new() -> Self {
        let config: DataSourceStatsdPluginConfig = match Config::get_pipeline_input_plugin_config()
        {
            Ok(c) => c.try_into().unwrap_or_else(|e| panic!("{}", e)),
            Err(_) => DataSourceStatsdPluginConfig {
                listen_address: Some("0.0.0.0:8125".to_string()),
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

    pub fn with_runtime_config(config: DataSourceStatsdPluginConfig) -> Self {
        Self {
            ingest: Ingest::new(),
            config,
        }
    }

    fn parse_statsd_line(line: &str) -> Option<String> {
        let parts: Vec<&str> = line.splitn(2, ':').collect();
        if parts.len() < 2 {
            return None;
        }
        let name = parts[0];
        let rest = parts[1];

        let segments: Vec<&str> = rest.split('|').collect();
        if segments.is_empty() {
            return None;
        }

        let value = segments[0];
        let metric_type = segments.get(1).unwrap_or(&"c");
        let mut sample_rate: Option<&str> = None;
        let mut tags: Vec<&str> = Vec::new();

        for seg in &segments[2..] {
            if let Some(sr) = seg.strip_prefix('@') {
                sample_rate = Some(sr);
            } else if let Some(t) = seg.strip_prefix('#') {
                tags = t.split(',').collect();
            }
        }

        let mut map = serde_json::Map::new();
        map.insert(
            "name".to_string(),
            serde_json::Value::String(name.to_string()),
        );
        map.insert(
            "value".to_string(),
            serde_json::Value::String(value.to_string()),
        );
        map.insert(
            "type".to_string(),
            serde_json::Value::String(metric_type.to_string()),
        );
        if let Some(sr) = sample_rate {
            map.insert(
                "sample_rate".to_string(),
                serde_json::Value::String(sr.to_string()),
            );
        }
        if !tags.is_empty() {
            let tag_vals: Vec<serde_json::Value> = tags
                .iter()
                .map(|t| serde_json::Value::String(t.to_string()))
                .collect();
            map.insert("tags".to_string(), serde_json::Value::Array(tag_vals));
        }

        Some(serde_json::to_string(&map).unwrap_or_default())
    }
}

#[async_trait]
impl DataSource for DataSourceStatsdPlugin {
    async fn sync(
        &mut self,
        offsets: Arc<Offsets>,
        shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
    ) -> Result<(), std::io::Error> {
        let addr = self
            .config
            .listen_address
            .clone()
            .unwrap_or_else(|| "0.0.0.0:8125".to_string());
        let socket = UdpSocket::bind(&addr).await?;
        info!("StatsD: listening on {}", addr);

        let mut buf = [0u8; 65535];
        let mut counter: u64 = 0;

        while RUNNING.read().load(Ordering::SeqCst) {
            match tokio::time::timeout(Duration::from_secs(1), socket.recv_from(&mut buf)).await {
                Ok(Ok((len, _addr))) => {
                    let raw = String::from_utf8_lossy(&buf[..len]).into_owned();
                    for line in raw.lines() {
                        if let Some(json_str) = Self::parse_statsd_line(line) {
                            counter += 1;
                            let bytes = json_str.len();
                            let offset_key = OffsetKey {
                                namespace: "statsd".to_string(),
                                partition: counter.to_string(),
                            };
                            let mut ingest_tasks = IngestTasks::new();
                            ingest_tasks.add(IngestTask::new(
                                vec![IngestBatch {
                                    offset_key,
                                    data: json_str,
                                    bytes,
                                    source_uri: format!("statsd://{}", addr),
                                    namespace: Some("statsd".to_string()),
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
                    }
                }
                Ok(Err(e)) => {
                    error!("StatsD recv error: {}", e);
                }
                Err(_) => continue,
            }
        }

        Ok(())
    }

    fn execution_contract(&self) -> skippr_runtime_sdk::plugins::SourceExecutionContract {
        skippr_runtime_sdk::plugins::SourceExecutionContract::stream(
            skippr_runtime_sdk::plugins::SourceOnceContract::HostIdleBounded,
        )
    }
}
