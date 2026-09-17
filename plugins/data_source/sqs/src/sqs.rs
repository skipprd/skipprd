use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_sdk_sqs::Client;
use serde_derive::Deserialize;
use tokio::time::{sleep, Duration};
use tracing::{error, info};

use crate::helpers::plugin_config::PluginConfigEntry;
use crate::RUNNING;
use skippr_runtime_sdk::plugins::{DataSource, SourceExecutionContract, SourceOnceContract};
use skippr_runtime_sdk::progress::OffsetKey;
use skippr_runtime_sdk::source_compat::{submit_payload_batches, IngestBatch, SourceSyncContext};

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
    pub(crate) config: DataSourceSqsPluginConfig,
    client: Client,
}

impl TryFrom<PluginConfigEntry> for DataSourceSqsPluginConfig {
    type Error = String;

    fn try_from(plugin_config: PluginConfigEntry) -> Result<Self, Self::Error> {
        plugin_config.decode_for_plugin("Sqs")
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
    async fn from_config(config: DataSourceSqsPluginConfig) -> Self {
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

        DataSourceSqsPlugin { config, client }
    }


    pub async fn with_runtime_config(config: DataSourceSqsPluginConfig) -> Self {
        Self::from_config(config).await
    }

    fn flush_pending(
        pending: &mut Vec<IngestBatch>,
        ctx: &dyn SourceSyncContext,
    ) -> Result<(), std::io::Error> {
        if pending.is_empty() {
            return Ok(());
        }
        let batch = std::mem::take(pending);
        submit_payload_batches(ctx, batch).map(|_| ())
    }

    async fn run_sync(&mut self, ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error> {
        let queue_url = self.config.queue_url.clone();
        if queue_url.is_empty() {
            error!("SQS queue_url is empty");
            return Ok(());
        }

        let stream_mode = self.config.mode.as_deref().unwrap_or("batch") == "stream";
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
                    Self::flush_pending(&mut pending, ctx.as_ref())?;
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
                let msg_id = msg.message_id().map(|s| s.to_string()).unwrap_or_default();

                let offset_key = OffsetKey {
                    namespace: offset_ns.clone(),
                    partition: msg_id.clone(),
                };

                pending.push(IngestBatch {
                    offset_key,
                    data: body,
                    bytes,
                    offset_pos: None,
                    source_uri: queue_url.clone(),
                    namespace: Some(ns_display.clone()),
                    cdc_rows: None,
                });
                pending_bytes += bytes;
                to_delete.push((msg_id, receipt));

                if pending_bytes >= batch_limit {
                    Self::flush_pending(&mut pending, ctx.as_ref())?;
                    pending_bytes = 0;
                    if !to_delete.is_empty() {
                        if let Err(e) =
                            Self::delete_batch(&self.client, &queue_url, &to_delete).await
                        {
                            error!("SQS delete_message_batch: {}", e);
                        }
                        to_delete.clear();
                    }
                }
            }

            if !pending.is_empty() {
                Self::flush_pending(&mut pending, ctx.as_ref())?;
                pending_bytes = 0;
            }

            if !to_delete.is_empty() {
                if let Err(e) = Self::delete_batch(&self.client, &queue_url, &to_delete).await {
                    error!("SQS delete_message_batch: {}", e);
                }
            }
        }

        if !pending.is_empty() {
            Self::flush_pending(&mut pending, ctx.as_ref())?;
        }

        info!("SQS input plugin sync complete");
        Ok(())
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
    async fn sync(&mut self, ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error> {
        self.run_sync(ctx).await
    }

    fn execution_contract(&self) -> SourceExecutionContract {
        if self.config.mode.as_deref().unwrap_or("batch") == "stream" {
            SourceExecutionContract::stream(SourceOnceContract::HostIdleBounded)
        } else {
            SourceExecutionContract::finite()
        }
    }
}
