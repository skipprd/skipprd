use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use rdkafka::config::ClientConfig;
use rdkafka::consumer::{Consumer, StreamConsumer};
use rdkafka::Message;
use serde_derive::Deserialize;
use tokio::time::{Duration, Instant};
use tracing::{error, info};

use crate::helpers::configuration::Config;
use crate::helpers::offsets::{OffsetKey, Offsets};
use crate::helpers::plugin_config::PluginConfigEntry;
use crate::ingest_work::{Ingest, IngestBatch, IngestTask, IngestTasks};
use crate::plugins::cdc::{source_capabilities, MutationKind, SourceCapability, WalRowMeta};
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
    pub cdc_enabled: Option<bool>,
    pub debezium_format: Option<bool>,
}

impl TryFrom<PluginConfigEntry> for DataSourceKafkaPluginConfig {
    type Error = String;

    fn try_from(plugin_config: PluginConfigEntry) -> Result<Self, Self::Error> {
        plugin_config.decode_for_plugin("Kafka")
    }
}

pub struct DataSourceKafkaPlugin {
    ingest: Ingest,
    config: DataSourceKafkaPluginConfig,
}

impl DataSourceKafkaPlugin {
    pub async fn new() -> Self {
        let config: DataSourceKafkaPluginConfig = match Config::get_pipeline_input_plugin_config() {
            Ok(c) => c.try_into().unwrap_or_else(|e| panic!("{}", e)),
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
                cdc_enabled: None,
                debezium_format: None,
            },
        };
        Self {
            ingest: Ingest::new(),
            config,
        }
    }

    pub fn with_runtime_config(config: DataSourceKafkaPluginConfig) -> Self {
        Self {
            ingest: Ingest::new(),
            config,
        }
    }

    fn is_cdc_enabled(&self) -> bool {
        self.config.cdc_enabled.unwrap_or(false)
    }

    fn make_event_id(topic: &str, partition: i32, offset: i64) -> Vec<u8> {
        format!("{}:{}:{}", topic, partition, offset).into_bytes()
    }

    fn parse_debezium_op(op: &str) -> MutationKind {
        match op {
            "c" => MutationKind::Insert,
            "u" => MutationKind::Update,
            "d" => MutationKind::Delete,
            "r" => MutationKind::Snapshot,
            _ => MutationKind::Insert,
        }
    }

    fn parse_debezium_envelope(
        payload: &str,
        topic: &str,
        partition: i32,
        offset: i64,
    ) -> (String, WalRowMeta) {
        let event_id = Self::make_event_id(topic, partition, offset);

        let parsed: Result<serde_json::Value, _> = serde_json::from_str(payload);
        let (data, mutation) = match parsed {
            Ok(envelope) => {
                let op = envelope.get("op").and_then(|v| v.as_str()).unwrap_or("c");
                let mutation = Self::parse_debezium_op(op);
                let row_data = if mutation == MutationKind::Delete {
                    envelope
                        .get("before")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null)
                } else {
                    envelope
                        .get("after")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null)
                };
                (
                    serde_json::to_string(&row_data).unwrap_or_else(|_| payload.to_string()),
                    mutation,
                )
            }
            Err(_) => (payload.to_string(), MutationKind::Insert),
        };

        let meta = WalRowMeta {
            mutation,
            event_id: event_id.clone(),
            order_token: event_id,
        };
        (data, meta)
    }
}

#[async_trait]
impl DataSource for DataSourceKafkaPlugin {
    async fn sync(
        &mut self,
        offsets: Arc<Offsets>,
        shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
    ) -> Result<(), std::io::Error> {
        let cdc = self.is_cdc_enabled();
        let debezium = self.config.debezium_format.unwrap_or(false);

        let group_id = self
            .config
            .group_id
            .clone()
            .unwrap_or_else(|| format!("skippr-{}", self.config.topic));
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
        let mut _counter: u64 = 0;

        info!(
            "Kafka: consuming from topic '{}' (mode: {}, cdc: {}, debezium: {})",
            self.config.topic,
            if is_batch { "batch" } else { "stream" },
            cdc,
            debezium,
        );

        let mut stream = consumer.stream();

        while RUNNING.read().load(Ordering::SeqCst) {
            match tokio::time::timeout(Duration::from_secs(1), stream.next()).await {
                Ok(Some(Ok(msg))) => {
                    last_msg = Instant::now();
                    _counter += 1;
                    let payload = match msg.payload_view::<str>() {
                        Some(Ok(s)) => s.to_string(),
                        Some(Err(_)) => {
                            let bytes = msg.payload().unwrap_or(&[]);
                            String::from_utf8_lossy(bytes).into_owned()
                        }
                        None => continue,
                    };

                    let offset_key = OffsetKey {
                        namespace: namespace.clone(),
                        partition: format!("{}:{}", msg.partition(), msg.offset()),
                    };

                    let (data, cdc_rows) = if cdc {
                        if debezium {
                            let (data, meta) = Self::parse_debezium_envelope(
                                &payload,
                                &self.config.topic,
                                msg.partition(),
                                msg.offset(),
                            );
                            (data, Some(vec![meta]))
                        } else {
                            let event_id = Self::make_event_id(
                                &self.config.topic,
                                msg.partition(),
                                msg.offset(),
                            );
                            let meta = WalRowMeta {
                                mutation: MutationKind::Insert,
                                event_id: event_id.clone(),
                                order_token: event_id,
                            };
                            (payload, Some(vec![meta]))
                        }
                    } else {
                        (payload, None)
                    };

                    let bytes = data.len();
                    let mut ingest_tasks = IngestTasks::new();
                    ingest_tasks.add(IngestTask::new(
                        vec![IngestBatch {
                            offset_key,
                            data,
                            bytes,
                            source_uri: format!(
                                "kafka://{}/{}",
                                self.config.brokers, self.config.topic
                            ),
                            namespace: Some(namespace.clone()),
                            cdc_rows,
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

    fn capability(&self) -> Option<&'static SourceCapability> {
        if self.is_cdc_enabled() {
            Some(&source_capabilities::KAFKA)
        } else {
            None
        }
    }
}
