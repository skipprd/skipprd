use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_sdk_sqs::Client;
use serde_derive::Deserialize;
use tokio::time::{sleep, Duration};
use tracing::{error, info};

use crate::helpers::configuration::{Config, DataSourcePluginConfig};
use crate::helpers::offsets::{OffsetKey, Offsets};
use crate::ingest_work::{Ingest, IngestBatch, IngestTask, IngestTasks};
use crate::plugins::{DataSink, DataSource};
use crate::RUNNING;

#[derive(Debug, Deserialize, Clone)]
pub struct DataSourceSqsPluginConfig {
    pub queue_url: String,
    pub region: Option<String>,
    pub endpoint_url: Option<String>,
    pub mode: Option<String>,
    pub format: Option<String>,
    pub batch_size_bytes: Option<i64>,
    pub batch_size_seconds: Option<i64>,
}

pub struct DataSourceSqsPlugin {
    pub(crate) ingest: Ingest,
    pub(crate) config: DataSourceSqsPluginConfig,
    client: Client,
}

impl From<DataSourcePluginConfig> for DataSourceSqsPluginConfig {
    fn from(plugin_config: DataSourcePluginConfig) -> Self {
        match plugin_config {
            DataSourcePluginConfig::Sqs(config) => config,
            _ => panic!("Invalid plugin type for SQS"),
        }
    }
}

fn queue_name_from_url(queue_url: &str) -> String {
    queue_url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("queue")
        .to_string()
}

impl DataSourceSqsPlugin {
    pub async fn new() -> Self {
        let config: DataSourceSqsPluginConfig = match Config::get_pipeline_input_plugin_config() {
            Ok(input_config) => input_config.into(),
            Err(_) => DataSourceSqsPluginConfig {
                queue_url: Config::getenv("SQS_QUEUE_URL", ""),
                region: {
                    let r = Config::getenv("AWS_DEFAULT_REGION", "");
                    if r.is_empty() {
                        None
                    } else {
                        Some(r)
                    }
                },
                endpoint_url: None,
                mode: None,
                format: None,
                batch_size_bytes: Some(
                    Config::getenv("DATA_SOURCE_BATCH_SIZE_BYTES", "1024000")
                        .parse::<i64>()
                        .unwrap_or(1_024_000),
                ),
                batch_size_seconds: Some(
                    Config::getenv("DATA_SOURCE_BATCH_SIZE_SECONDS", "600")
                        .parse::<i64>()
                        .unwrap_or(600),
                ),
            },
        };

        let mut loader = aws_config::defaults(BehaviorVersion::latest());
        if let Some(ref region) = config.region {
            loader = loader.region(aws_types::region::Region::new(region.clone()));
        }
        let conf = loader.load().await;
        let mut client_config = aws_sdk_sqs::config::Builder::from(&conf);
        if let Some(ref endpoint_url) = config.endpoint_url {
            client_config = client_config.endpoint_url(endpoint_url);
        }
        let client = Client::from_conf(client_config.build());

        DataSourceSqsPlugin {
            ingest: Ingest::new(),
            config,
            client,
        }
    }

    fn flush_pending(
        &mut self,
        pending: &mut Vec<IngestBatch>,
        offsets: &Arc<Offsets>,
        output: &Arc<Box<dyn DataSink + Send + Sync>>,
    ) {
        if pending.is_empty() {
            return;
        }
        let batch = std::mem::take(pending);
        let mut tasks = IngestTasks::new();
        tasks.add(IngestTask::new(
            batch,
            offsets.clone(),
            output.clone(),
        ));
        self.ingest
            .ingest_file(&Arc::new(tasks), offsets, output.clone());
    }

    pub async fn sync(
        &mut self,
        offsets: Arc<Offsets>,
        shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
    ) {
        let queue_url = self.config.queue_url.clone();
        if queue_url.is_empty() {
            error!("SQS queue_url is empty");
            return;
        }

        let stream_mode = self
            .config
            .mode
            .as_deref()
            .unwrap_or("batch")
            == "stream";
        let batch_limit = self.config.batch_size_bytes.unwrap_or(1_024_000).max(1) as usize;
        let qname = queue_name_from_url(&queue_url);
        let ns_display = format!("sqs.{qname}");
        let offset_ns = format!("sqs:{queue_url}");

        let mut pending: Vec<IngestBatch> = Vec::new();
        let mut pending_bytes: usize = 0;
        let mut backoff_ms: u64 = 100;

        loop {
            if !RUNNING.read().load(Ordering::SeqCst) {
                break;
            }

            let resp = match self
                .client
                .receive_message()
                .queue_url(&queue_url)
                .max_number_of_messages(10)
                .wait_time_seconds(20)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    error!("SQS receive_message: {}", e);
                    if stream_mode {
                        sleep(Duration::from_millis(backoff_ms)).await;
                        backoff_ms = (backoff_ms * 2).min(5000);
                    } else {
                        break;
                    }
                    continue;
                }
            };

            let messages = resp.messages.unwrap_or_default();
            if messages.is_empty() {
                if !pending.is_empty() {
                    self.flush_pending(&mut pending, &offsets, &shared_output);
                    pending_bytes = 0;
                }
                if stream_mode {
                    sleep(Duration::from_millis(backoff_ms)).await;
                    backoff_ms = (backoff_ms * 2).min(5000);
                    continue;
                } else {
                    break;
                }
            }

            backoff_ms = 100;

            let mut to_delete: Vec<(String, String)> = Vec::new();

            for msg in &messages {
                let Some(receipt) = msg.receipt_handle().map(|s| s.to_string()) else {
                    error!("SQS message missing receipt_handle; skipping");
                    continue;
                };
                let body = msg.body().unwrap_or("").to_string();
                let bytes = body.len();
                let msg_id = msg
                    .message_id()
                    .map(|s| s.to_string())
                    .unwrap_or_default();

                let offset_key = OffsetKey {
                    namespace: offset_ns.clone(),
                    partition: msg_id.clone(),
                };

                pending.push(IngestBatch {
                    offset_key,
                    data: body,
                    bytes,
                    source_uri: queue_url.clone(),
                    namespace: Some(ns_display.clone()),
                });
                pending_bytes += bytes;
                to_delete.push((msg_id, receipt));

                if pending_bytes >= batch_limit {
                    self.flush_pending(&mut pending, &offsets, &shared_output);
                    pending_bytes = 0;
                    if !to_delete.is_empty() {
                        if let Err(e) = Self::delete_batch(&self.client, &queue_url, &to_delete).await
                        {
                            error!("SQS delete_message_batch: {}", e);
                        }
                        to_delete.clear();
                    }
                }
            }

            if !pending.is_empty() {
                self.flush_pending(&mut pending, &offsets, &shared_output);
                pending_bytes = 0;
            }

            if !to_delete.is_empty() {
                if let Err(e) = Self::delete_batch(&self.client, &queue_url, &to_delete).await {
                    error!("SQS delete_message_batch: {}", e);
                }
            }
        }

        if !pending.is_empty() {
            self.flush_pending(&mut pending, &offsets, &shared_output);
        }

        info!("SQS input plugin sync complete");
    }

    async fn delete_batch(
        client: &Client,
        queue_url: &str,
        entries: &[(String, String)],
    ) -> Result<(), std::io::Error> {
        for chunk in entries.chunks(10) {
            let batch: Vec<aws_sdk_sqs::types::DeleteMessageBatchRequestEntry> = chunk
                .iter()
                .enumerate()
                .map(|(i, (_msg_id, rh))| {
                    aws_sdk_sqs::types::DeleteMessageBatchRequestEntry::builder()
                        .id(i.to_string())
                        .receipt_handle(rh)
                        .build()
                        .map_err(|e| std::io::Error::other(e.to_string()))
                })
                .collect::<Result<Vec<_>, _>>()?;
            client
                .delete_message_batch()
                .queue_url(queue_url)
                .set_entries(Some(batch))
                .send()
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        }
        Ok(())
    }
}

#[async_trait]
impl DataSource for DataSourceSqsPlugin {
    async fn sync(
        &mut self,
        offsets: Arc<Offsets>,
        output: Arc<Box<dyn DataSink + Send + Sync>>,
    ) -> Result<(), std::io::Error> {
        self.sync(offsets, output).await;
        Ok(())
    }
}
