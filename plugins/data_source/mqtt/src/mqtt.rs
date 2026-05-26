use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use serde_derive::Deserialize;
use tokio::time::{timeout, Duration, Instant};
use tracing::{error, info};

use crate::helpers::configuration::Config;
use crate::helpers::plugin_config::PluginConfigEntry;
use crate::RUNNING;
use skippr_runtime_sdk::plugins::{DataSource, SourceExecutionContract, SourceOnceContract};
use skippr_runtime_sdk::progress::OffsetKey;
use skippr_runtime_sdk::source_compat::{submit_payload_batches, IngestBatch, SourceSyncContext};

#[derive(Debug, Deserialize, Clone)]
pub struct DataSourceMqttPluginConfig {
    pub broker_url: String,
    pub port: Option<u16>,
    pub topic: String,
    pub client_id: Option<String>,
    pub qos: Option<u8>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub mode: Option<String>,
    pub idle_timeout_seconds: Option<u64>,
    pub format: Option<String>,
    pub batch_size_bytes: Option<i64>,
    pub batch_size_seconds: Option<i64>,
}

impl TryFrom<PluginConfigEntry> for DataSourceMqttPluginConfig {
    type Error = String;

    fn try_from(plugin_config: PluginConfigEntry) -> Result<Self, Self::Error> {
        plugin_config.decode_for_plugin("Mqtt")
    }
}

pub struct DataSourceMqttPlugin {
    config: DataSourceMqttPluginConfig,
}

impl DataSourceMqttPlugin {
    pub async fn new() -> Self {
        let config: DataSourceMqttPluginConfig = match Config::get_pipeline_input_plugin_config() {
            Ok(c) => c.try_into().unwrap_or_else(|e| panic!("{}", e)),
            Err(_) => DataSourceMqttPluginConfig {
                broker_url: Config::getenv("MQTT_BROKER_URL", ""),
                port: None,
                topic: Config::getenv("MQTT_TOPIC", ""),
                client_id: None,
                qos: None,
                username: None,
                password: None,
                mode: None,
                idle_timeout_seconds: None,
                format: None,
                batch_size_bytes: None,
                batch_size_seconds: None,
            },
        };
        Self { config }
    }

    pub fn with_runtime_config(config: DataSourceMqttPluginConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl DataSource for DataSourceMqttPlugin {
    async fn sync(&mut self, ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error> {
        let client_id = self
            .config
            .client_id
            .clone()
            .unwrap_or_else(|| format!("skippr-{}", uuid::Uuid::new_v4()));
        let port = self.config.port.unwrap_or(1883);
        let mut mqttoptions = MqttOptions::new(&client_id, &self.config.broker_url, port);
        mqttoptions.set_keep_alive(Duration::from_secs(30));

        if let (Some(ref user), Some(ref pass)) = (&self.config.username, &self.config.password) {
            mqttoptions.set_credentials(user, pass);
        }

        let (client, mut eventloop) = AsyncClient::new(mqttoptions, 100);
        let qos = match self.config.qos.unwrap_or(1) {
            0 => QoS::AtMostOnce,
            2 => QoS::ExactlyOnce,
            _ => QoS::AtLeastOnce,
        };

        client
            .subscribe(&self.config.topic, qos)
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let namespace = format!("mqtt.{}", self.config.topic);
        let is_batch = self.config.mode.as_deref() == Some("batch");
        let idle_timeout_dur = Duration::from_secs(self.config.idle_timeout_seconds.unwrap_or(5));
        let mut last_msg = Instant::now();
        let mut counter: u64 = 0;

        info!(
            "MQTT: subscribed to '{}' (mode: {})",
            self.config.topic,
            if is_batch { "batch" } else { "stream" }
        );

        while RUNNING.read().load(Ordering::SeqCst) {
            match timeout(Duration::from_secs(1), eventloop.poll()).await {
                Ok(Ok(Event::Incoming(Packet::Publish(publish)))) => {
                    last_msg = Instant::now();
                    counter += 1;
                    let data = String::from_utf8_lossy(&publish.payload).into_owned();
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
                            source_uri: format!(
                                "mqtt://{}/{}",
                                self.config.broker_url, self.config.topic
                            ),
                            namespace: Some(namespace.clone()),
                            cdc_rows: None,
                        }],
                    )?;
                }
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    error!("MQTT error: {}", e);
                    break;
                }
                Err(_) => {
                    if is_batch && last_msg.elapsed() > idle_timeout_dur {
                        info!("MQTT batch mode: idle timeout reached, exiting");
                        break;
                    }
                }
            }
        }

        let _ = client.disconnect().await;
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
