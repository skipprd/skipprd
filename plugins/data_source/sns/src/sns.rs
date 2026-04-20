use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_sdk_sqs::Client as SqsClient;
use serde_derive::Deserialize;
use tokio::time::{sleep, Duration};
use tracing::info;

use crate::helpers::configuration::Config;
use crate::helpers::offsets::{OffsetKey, Offsets};
use crate::helpers::plugin_config::PluginConfigEntry;
use crate::ingest_work::{Ingest, IngestBatch, IngestTask, IngestTasks};
use crate::plugins::{DataSink, DataSource};
use crate::RUNNING;

#[derive(Debug, Deserialize, Clone)]
pub struct DataSourceSnsPluginConfig {
    pub topic_arn: String,
    pub sqs_queue_url: String,
    pub region: Option<String>,
    pub endpoint_url: Option<String>,
    pub format: Option<String>,
    pub batch_size_bytes: Option<i64>,
    pub batch_size_seconds: Option<i64>,
}

impl TryFrom<PluginConfigEntry> for DataSourceSnsPluginConfig {
    type Error = String;

    fn try_from(plugin_config: PluginConfigEntry) -> Result<Self, Self::Error> {
        plugin_config.decode_for_plugin("Sns")
    }
}

pub struct DataSourceSnsPlugin {
    ingest: Ingest,
    config: DataSourceSnsPluginConfig,
    sqs_client: SqsClient,
}

impl DataSourceSnsPlugin {
    async fn from_config(config: DataSourceSnsPluginConfig) -> Self {
        let mut aws_builder = aws_config::defaults(BehaviorVersion::latest());
        if let Some(ref region) = config.region {
            aws_builder = aws_builder.region(aws_config::Region::new(region.clone()));
        }
        let aws_config = aws_builder.load().await;
        let mut sqs_builder = aws_sdk_sqs::config::Builder::from(&aws_config);
        if let Some(ref endpoint) = config.endpoint_url {
            sqs_builder = sqs_builder.endpoint_url(endpoint);
        }
        let sqs_client = SqsClient::from_conf(sqs_builder.build());

        Self {
            ingest: Ingest::new(),
            config,
            sqs_client,
        }
    }

    pub async fn new() -> Self {
        let config: DataSourceSnsPluginConfig = match Config::get_pipeline_input_plugin_config() {
            Ok(c) => c.try_into().unwrap_or_else(|e| panic!("{}", e)),
            Err(_) => DataSourceSnsPluginConfig {
                topic_arn: Config::getenv("SNS_TOPIC_ARN", ""),
                sqs_queue_url: Config::getenv("SNS_SQS_QUEUE_URL", ""),
                region: None,
                endpoint_url: None,
                format: None,
                batch_size_bytes: None,
                batch_size_seconds: None,
            },
        };

        Self::from_config(config).await
    }

    pub async fn with_runtime_config(config: DataSourceSnsPluginConfig) -> Self {
        Self::from_config(config).await
    }

    fn extract_topic_name(arn: &str) -> String {
        arn.rsplit(':').next().unwrap_or("topic").to_string()
    }
}

#[async_trait]
impl DataSource for DataSourceSnsPlugin {
    async fn sync(
        &mut self,
        offsets: Arc<Offsets>,
        shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
    ) -> Result<(), std::io::Error> {
        let topic_name = Self::extract_topic_name(&self.config.topic_arn);
        let namespace = format!("sns.{}", topic_name);
        info!("SNS: consuming from SQS queue for topic '{}'", topic_name);

        while RUNNING.read().load(Ordering::SeqCst) {
            let result = self
                .sqs_client
                .receive_message()
                .queue_url(&self.config.sqs_queue_url)
                .max_number_of_messages(10)
                .wait_time_seconds(5)
                .send()
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?;

            let messages = result.messages.unwrap_or_default();
            if messages.is_empty() {
                sleep(Duration::from_secs(1)).await;
                continue;
            }

            let mut pending: Vec<IngestBatch> = Vec::new();

            for msg in &messages {
                let body = match msg.body() {
                    Some(b) => b.to_string(),
                    None => continue,
                };
                let msg_id = msg.message_id().unwrap_or("unknown").to_string();

                let inner = match serde_json::from_str::<serde_json::Value>(&body) {
                    Ok(v) => match v.get("Message") {
                        Some(serde_json::Value::String(s)) => s.clone(),
                        _ => body.clone(),
                    },
                    Err(_) => body.clone(),
                };

                let bytes = inner.len();
                let offset_key = OffsetKey {
                    namespace: namespace.clone(),
                    partition: msg_id,
                };

                pending.push(IngestBatch {
                    offset_key,
                    data: inner,
                    bytes,
                    source_uri: format!("sns://{}", topic_name),
                    namespace: Some(namespace.clone()),
                    cdc_rows: None,
                });
            }

            if !pending.is_empty() {
                let mut ingest_tasks = IngestTasks::new();
                ingest_tasks.add(IngestTask::new(
                    pending,
                    offsets.clone(),
                    shared_output.clone(),
                ));
                self.ingest
                    .ingest_file(&Arc::new(ingest_tasks), &offsets, shared_output.clone());
            }

            for msg in &messages {
                if let Some(handle) = msg.receipt_handle() {
                    let _ = self
                        .sqs_client
                        .delete_message()
                        .queue_url(&self.config.sqs_queue_url)
                        .receipt_handle(handle)
                        .send()
                        .await;
                }
            }
        }

        Ok(())
    }
}
