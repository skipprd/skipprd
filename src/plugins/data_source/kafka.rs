use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::StreamExt;
use rdkafka::config::ClientConfig;
use rdkafka::consumer::{Consumer, StreamConsumer};
use rdkafka::Message;
use serde_derive::Deserialize;
use tokio::time::{Duration, Instant};
use tracing::{error, info};

use crate::helpers::configuration::{Config, DataSourcePluginConfig};
use crate::helpers::offsets::{OffsetKey, Offsets};
use crate::ingest_work::{Ingest, IngestBatch, IngestTask, IngestTasks};
use crate::plugins::{DataSink, DataSource};
use crate::RUNNING;

#[derive(Debug, Deserialize, Clone)]
pub struct DataSourceKafkaPluginConfig {
    pub brokers: String,
    pub topic: String,
    pub group_id: Option<String>,
    pub auto_offset_reset: Option<String>,
    pub security_protocol: Option<String>,
    pub sasl_mechanism: Option<String>,
    pub sasl_username: Option<String>,
    pub sasl_password: Option<String>,
    pub mode: Option<String>,
    pub idle_timeout_seconds: Option<u64>,
    pub format: Option<String>,
    pub batch_size_bytes: Option<i64>,
    pub batch_size_seconds: Option<i64>,
}

impl From<DataSourcePluginConfig> for DataSourceKafkaPluginConfig {
    fn from(plugin_config: DataSourcePluginConfig) -> Self {
        match plugin_config {
            DataSourcePluginConfig::Kafka(config) => config,
            _ => panic!("Invalid plugin type for Kafka"),
        }
    }
}

pub struct DataSourceKafkaPlugin {
    ingest: Ingest,
    config: DataSourceKafkaPluginConfig,
}

impl DataSourceKafkaPlugin {
    pub async fn new() -> Self {
        let config: DataSourceKafkaPluginConfig = match Config::get_pipeline_input_plugin_config() {
            Ok(c) => c.into(),
            Err(_) => DataSourceKafkaPluginConfig {
                brokers: Config::getenv("KAFKA_BROKERS", ""),
                topic: Config::getenv("KAFKA_TOPIC", ""),
                group_id: None,
                auto_offset_reset: None,
                security_protocol: None,
                sasl_mechanism: None,
                sasl_username: None,
                sasl_password: None,
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
}

#[async_trait]
impl DataSource for DataSourceKafkaPlugin {
    async fn sync(
        &mut self,
        offsets: Arc<Offsets>,
        shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
    ) -> Result<(), std::io::Error> {
        let group_id = self
            .config
            .group_id
            .clone()
            .unwrap_or_else(|| format!("skippr-{}", uuid::Uuid::new_v4()));
        let offset_reset = self
            .config
            .auto_offset_reset
            .as_deref()
            .unwrap_or("earliest");

        let mut kafka_config = ClientConfig::new();
        kafka_config
            .set("bootstrap.servers", &self.config.brokers)
            .set("group.id", &group_id)
            .set("auto.offset.reset", offset_reset)
            .set("enable.auto.commit", "false");

        if let Some(ref protocol) = self.config.security_protocol {
            kafka_config.set("security.protocol", protocol);
        }
        if let Some(ref mechanism) = self.config.sasl_mechanism {
            kafka_config.set("sasl.mechanism", mechanism);
        }
        if let Some(ref username) = self.config.sasl_username {
            kafka_config.set("sasl.username", username);
        }
        if let Some(ref password) = self.config.sasl_password {
            kafka_config.set("sasl.password", password);
        }

        let consumer: StreamConsumer = kafka_config
            .create()
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        consumer
            .subscribe(&[&self.config.topic])
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let namespace = format!("kafka.{}", self.config.topic);
        let is_batch = self.config.mode.as_deref() == Some("batch");
        let idle_timeout_dur = Duration::from_secs(self.config.idle_timeout_seconds.unwrap_or(5));
        let mut last_msg = Instant::now();
        let mut counter: u64 = 0;

        info!(
            "Kafka: consuming from topic '{}' (mode: {})",
            self.config.topic,
            if is_batch { "batch" } else { "stream" }
        );

        let mut stream = consumer.stream();

        while RUNNING.read().load(Ordering::SeqCst) {
            match tokio::time::timeout(Duration::from_secs(1), stream.next()).await {
                Ok(Some(Ok(msg))) => {
                    last_msg = Instant::now();
                    counter += 1;
                    let payload = match msg.payload_view::<str>() {
                        Some(Ok(s)) => s.to_string(),
                        Some(Err(_)) => {
                            let bytes = msg.payload().unwrap_or(&[]);
                            String::from_utf8_lossy(bytes).into_owned()
                        }
                        None => continue,
                    };
                    let bytes = payload.len();
                    let offset_key = OffsetKey {
                        namespace: namespace.clone(),
                        partition: format!("{}:{}", msg.partition(), msg.offset()),
                    };
                    let mut ingest_tasks = IngestTasks::new();
                    ingest_tasks.add(IngestTask::new(
                        vec![IngestBatch {
                            offset_key,
                            data: payload,
                            bytes,
                            source_uri: format!(
                                "kafka://{}/{}",
                                self.config.brokers, self.config.topic
                            ),
                            namespace: Some(namespace.clone()),
                        }],
                        offsets.clone(),
                        shared_output.clone(),
                    ));
                    self.ingest.ingest_file(
                        &Arc::new(ingest_tasks),
                        &offsets,
                        shared_output.clone(),
                    );
                    if let Err(e) =
                        consumer.commit_message(&msg, rdkafka::consumer::CommitMode::Async)
                    {
                        error!("Kafka commit error: {}", e);
                    }
                }
                Ok(Some(Err(e))) => {
                    error!("Kafka consumer error: {}", e);
                }
                Ok(None) => break,
                Err(_) => {
                    if is_batch && last_msg.elapsed() > idle_timeout_dur {
                        info!("Kafka batch mode: idle timeout reached, exiting");
                        break;
                    }
                }
            }
        }

        Ok(())
    }
}
