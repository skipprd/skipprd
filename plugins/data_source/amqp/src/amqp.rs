use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use lapin::{options::*, types::FieldTable, Connection, ConnectionProperties};
use serde_derive::Deserialize;
use tokio::time::{Duration, Instant};
use tracing::{error, info};

use crate::helpers::plugin_config::PluginConfigEntry;
use crate::RUNNING;
use skippr_runtime_sdk::plugins::{DataSource, SourceExecutionContract, SourceOnceContract};
use skippr_runtime_sdk::progress::OffsetKey;
use skippr_runtime_sdk::source_compat::{submit_payload_batches, IngestBatch, SourceSyncContext};

#[derive(Debug, Deserialize, Clone)]
pub struct DataSourceAmqpPluginConfig {
    pub connection_string: String,
    pub queue: String,
    pub exchange: Option<String>,
    pub routing_key: Option<String>,
    pub consumer_tag: Option<String>,
    pub prefetch_count: Option<u16>,
    pub mode: Option<String>,
    pub idle_timeout_seconds: Option<u64>,
    pub format: Option<String>,
    pub batch_size_bytes: Option<i64>,
    pub batch_size_seconds: Option<i64>,
}

impl TryFrom<PluginConfigEntry> for DataSourceAmqpPluginConfig {
    type Error = String;

    fn try_from(plugin_config: PluginConfigEntry) -> Result<Self, Self::Error> {
        plugin_config.decode_for_plugin("Amqp")
    }
}

pub struct DataSourceAmqpPlugin {
    config: DataSourceAmqpPluginConfig,
}

impl DataSourceAmqpPlugin {

    pub fn with_runtime_config(config: DataSourceAmqpPluginConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl DataSource for DataSourceAmqpPlugin {
    async fn sync(&mut self, ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error> {
        let conn = Connection::connect(
            &self.config.connection_string,
            ConnectionProperties::default(),
        )
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;
        let channel = conn
            .create_channel()
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let prefetch = self.config.prefetch_count.unwrap_or(10);
        channel
            .basic_qos(prefetch, BasicQosOptions::default())
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        channel
            .queue_declare(
                &self.config.queue,
                QueueDeclareOptions {
                    durable: true,
                    ..Default::default()
                },
                FieldTable::default(),
            )
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        if let (Some(ref exchange), Some(ref routing_key)) =
            (&self.config.exchange, &self.config.routing_key)
        {
            channel
                .queue_bind(
                    &self.config.queue,
                    exchange,
                    routing_key,
                    QueueBindOptions::default(),
                    FieldTable::default(),
                )
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        }

        let consumer_tag = self
            .config
            .consumer_tag
            .clone()
            .unwrap_or_else(|| format!("skippr-{}", uuid::Uuid::new_v4()));

        let mut consumer = channel
            .basic_consume(
                &self.config.queue,
                &consumer_tag,
                BasicConsumeOptions::default(),
                FieldTable::default(),
            )
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let namespace = format!("amqp.{}", self.config.queue);
        let is_batch = self.config.mode.as_deref() == Some("batch");
        let idle_timeout_dur = Duration::from_secs(self.config.idle_timeout_seconds.unwrap_or(5));
        let mut last_msg = Instant::now();
        let mut counter: u64 = 0;

        info!(
            "AMQP: consuming from queue '{}' (mode: {})",
            self.config.queue,
            if is_batch { "batch" } else { "stream" }
        );

        while RUNNING.read().load(Ordering::SeqCst) {
            match tokio::time::timeout(Duration::from_secs(1), consumer.next()).await {
                Ok(Some(Ok(delivery))) => {
                    last_msg = Instant::now();
                    counter += 1;
                    let data = String::from_utf8_lossy(&delivery.data).into_owned();
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
                            source_uri: format!("amqp://{}", self.config.queue),
                            namespace: Some(namespace.clone()),
                            cdc_rows: None,
                        }],
                    )?;
                    delivery
                        .ack(BasicAckOptions::default())
                        .await
                        .map_err(|e| std::io::Error::other(e.to_string()))?;
                }
                Ok(Some(Err(e))) => {
                    error!("AMQP consumer error: {}", e);
                    break;
                }
                Ok(None) => break,
                Err(_) => {
                    if is_batch && last_msg.elapsed() > idle_timeout_dur {
                        info!("AMQP batch mode: idle timeout reached, exiting");
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
