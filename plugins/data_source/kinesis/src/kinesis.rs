use skippr_runtime_sdk::SkipprConfig;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_sdk_kinesis::types::ShardIteratorType;
use aws_sdk_kinesis::Client;
use serde_derive::{Deserialize, Serialize};
use tokio::time::{sleep, Duration};
use tracing::{error, info};

use crate::helpers::plugin_config::PluginConfigEntry;
use crate::RUNNING;
use skippr_runtime_sdk::plugins::{DataSource, SourceExecutionContract, SourceOnceContract};
use skippr_runtime_sdk::progress::OffsetKey;
use skippr_runtime_sdk::source_compat::{
    load_checkpoint_payload, store_checkpoint_payload, submit_payload_batches, IngestBatch,
    SourceSyncContext,
};

#[derive(Debug, Serialize, Deserialize)]
struct KinesisShardCheckpoint {
    seq: String,
}

#[derive(Debug, Deserialize, SkipprConfig, Clone)]
pub struct DataSourceKinesisPluginConfig {
    pub stream_name: String,
    pub region: Option<String>,
    pub endpoint_url: Option<String>,
    pub mode: Option<String>,
    pub format: Option<String>,
    pub batch_size_bytes: Option<i64>,
    pub batch_size_seconds: Option<i64>,
}

pub struct DataSourceKinesisPlugin {
    pub(crate) config: DataSourceKinesisPluginConfig,
    client: Client,
}

impl TryFrom<PluginConfigEntry> for DataSourceKinesisPluginConfig {
    type Error = String;

    fn try_from(plugin_config: PluginConfigEntry) -> Result<Self, Self::Error> {
        plugin_config.decode_for_plugin("Kinesis")
    }
}

impl DataSourceKinesisPlugin {
    fn checkpoint_key(stream: &str, shard_id: &str) -> String {
        format!("kinesis:{stream}:{shard_id}")
    }

    fn read_checkpoint(
        ctx: &dyn SourceSyncContext,
        stream: &str,
        shard_id: &str,
    ) -> Option<String> {
        load_checkpoint_payload::<KinesisShardCheckpoint>(
            ctx,
            &Self::checkpoint_key(stream, shard_id),
        )
        .ok()
        .flatten()
        .map(|p| p.seq)
        .filter(|s| !s.is_empty())
    }

    fn write_checkpoint(
        ctx: &dyn SourceSyncContext,
        stream: &str,
        shard_id: &str,
        seq: &str,
    ) -> std::io::Result<()> {
        store_checkpoint_payload(
            ctx,
            &Self::checkpoint_key(stream, shard_id),
            &KinesisShardCheckpoint {
                seq: seq.to_string(),
            },
        )
    }

    async fn new_shard_iterator(
        client: &Client,
        stream_name: &str,
        shard_id: &str,
        after_seq: Option<&str>,
    ) -> Result<String, std::io::Error> {
        let out = if let Some(seq) = after_seq {
            client
                .get_shard_iterator()
                .stream_name(stream_name)
                .shard_id(shard_id)
                .shard_iterator_type(ShardIteratorType::AfterSequenceNumber)
                .starting_sequence_number(seq)
                .send()
                .await
        } else {
            client
                .get_shard_iterator()
                .stream_name(stream_name)
                .shard_id(shard_id)
                .shard_iterator_type(ShardIteratorType::TrimHorizon)
                .send()
                .await
        }
        .map_err(|e| std::io::Error::other(e.to_string()))?;
        out.shard_iterator()
            .map(|s| s.to_string())
            .ok_or_else(|| std::io::Error::other("missing shard_iterator"))
    }

    async fn from_config(config: DataSourceKinesisPluginConfig) -> Self {
        let mut loader = aws_config::defaults(BehaviorVersion::latest());
        if let Some(ref region) = config.region {
            loader = loader.region(aws_types::region::Region::new(region.clone()));
        }
        let conf = loader.load().await;
        let mut client_config = aws_sdk_kinesis::config::Builder::from(&conf);
        if let Some(ref endpoint_url) = config.endpoint_url {
            client_config = client_config.endpoint_url(endpoint_url);
        }
        let client = Client::from_conf(client_config.build());

        DataSourceKinesisPlugin { config, client }
    }

    pub async fn with_runtime_config(config: DataSourceKinesisPluginConfig) -> Self {
        Self::from_config(config).await
    }

    fn flush_shard_buffer(
        &self,
        stream_name: &str,
        shard_id: &str,
        pending: &mut Vec<IngestBatch>,
        last_seq: &mut Option<String>,
        ctx: &dyn SourceSyncContext,
    ) -> Result<(), std::io::Error> {
        if pending.is_empty() {
            return Ok(());
        }
        let batch = std::mem::take(pending);
        if let Some(last) = batch.last().and_then(|b| {
            let p = b.offset_key().partition();
            if p.is_empty() {
                None
            } else {
                Some(p.to_string())
            }
        }) {
            *last_seq = Some(last);
        }
        submit_payload_batches(ctx, batch)?;
        let seq = last_seq.clone().unwrap_or_default();
        if !seq.is_empty() {
            Self::write_checkpoint(ctx, stream_name, shard_id, &seq)?;
        }
        Ok(())
    }

    async fn list_all_shards(&self) -> Result<Vec<String>, std::io::Error> {
        let mut shards = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let out = if let Some(ref t) = token {
                self.client.list_shards().next_token(t).send().await
            } else {
                self.client
                    .list_shards()
                    .stream_name(self.config.stream_name.clone())
                    .send()
                    .await
            }
            .map_err(|e| std::io::Error::other(e.to_string()))?;
            for s in out.shards() {
                shards.push(s.shard_id().to_string());
            }
            token = out.next_token().map(|t| t.to_string());
            if token.is_none() {
                break;
            }
        }
        Ok(shards)
    }

    async fn run_sync(&mut self, ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error> {
        let stream_name = self.config.stream_name.clone();
        if stream_name.is_empty() {
            error!("Kinesis stream_name is empty");
            return Ok(());
        }

        let stream_mode = self.config.mode.as_deref().unwrap_or("batch") == "stream";
        let batch_limit = self.config.batch_size_bytes.unwrap_or(1_024_000).max(1) as usize;
        let ns_display = format!("kinesis.{}", stream_name);

        let shard_ids = match self.list_all_shards().await {
            Ok(s) => s,
            Err(e) => {
                error!("Kinesis list_shards failed: {}", e);
                return Ok(());
            }
        };

        if shard_ids.is_empty() {
            info!("Kinesis stream {} has no shards", stream_name);
            return Ok(());
        }

        let mut iterators: HashMap<String, String> = HashMap::new();
        for sid in &shard_ids {
            let ckpt = Self::read_checkpoint(ctx.as_ref(), &stream_name, sid);
            match Self::new_shard_iterator(&self.client, &stream_name, sid, ckpt.as_deref()).await {
                Ok(it) => {
                    iterators.insert(sid.clone(), it);
                }
                Err(e) => {
                    error!("Kinesis get_shard_iterator for {}: {}", sid, e);
                }
            }
        }

        let mut buffers: HashMap<String, Vec<IngestBatch>> = HashMap::new();
        let mut buffer_bytes: HashMap<String, usize> = HashMap::new();
        let mut last_seq_per_shard: HashMap<String, Option<String>> = HashMap::new();
        for sid in &shard_ids {
            buffers.insert(sid.clone(), Vec::new());
            buffer_bytes.insert(sid.clone(), 0);
            last_seq_per_shard.insert(sid.clone(), None);
        }

        let mut backoff_ms: u64 = 100;

        loop {
            if !RUNNING.read().load(Ordering::SeqCst) {
                break;
            }

            let mut round_had_records = false;
            let mut round_all_millis_zero = true;

            for sid in &shard_ids {
                let Some(iter) = iterators.get(sid).cloned() else {
                    continue;
                };

                let resp = match self.client.get_records().shard_iterator(&iter).send().await {
                    Ok(r) => r,
                    Err(e) => {
                        error!("Kinesis get_records {}: {}", sid, e);
                        let ckpt = Self::read_checkpoint(ctx.as_ref(), &stream_name, sid);
                        match Self::new_shard_iterator(
                            &self.client,
                            &stream_name,
                            sid,
                            ckpt.as_deref(),
                        )
                        .await
                        {
                            Ok(new_it) => {
                                iterators.insert(sid.clone(), new_it);
                            }
                            Err(e2) => {
                                error!("Kinesis iterator refresh failed {}: {}", sid, e2);
                            }
                        }
                        continue;
                    }
                };

                if let Some(next) = resp.next_shard_iterator() {
                    iterators.insert(sid.clone(), next.to_string());
                }

                let behind = resp.millis_behind_latest().unwrap_or(0);
                if behind > 0 {
                    round_all_millis_zero = false;
                }

                let records = resp.records;
                if !records.is_empty() {
                    round_had_records = true;
                    round_all_millis_zero = false;
                }

                let buf = buffers.get_mut(sid).unwrap();
                let bbytes = buffer_bytes.get_mut(sid).unwrap();
                let last_seq = last_seq_per_shard.get_mut(sid).unwrap();

                for rec in records {
                    let seq = rec.sequence_number().to_string();
                    let data = rec.data().as_ref().to_vec();
                    let bytes = data.len();
                    let offset_key = OffsetKey {
                        namespace: format!("kinesis:{stream_name}:{sid}"),
                        partition: seq.clone(),
                    };
                    buf.push(IngestBatch {
                        offset_key,
                        data,
                        bytes,
                        offset_pos: None,
                        source_uri: format!("kinesis://{stream_name}/{sid}"),
                        namespace: Some(ns_display.clone()),
                        cdc_rows: None,
                    });
                    *bbytes += bytes;
                    *last_seq = Some(seq);

                    if *bbytes >= batch_limit {
                        self.flush_shard_buffer(&stream_name, sid, buf, last_seq, ctx.as_ref())?;
                        *bbytes = 0;
                    }
                }
            }

            if !round_had_records && !stream_mode {
                for sid in &shard_ids {
                    let buf = buffers.get_mut(sid).unwrap();
                    let last_seq = last_seq_per_shard.get_mut(sid).unwrap();
                    if !buf.is_empty() {
                        self.flush_shard_buffer(&stream_name, sid, buf, last_seq, ctx.as_ref())?;
                    }
                    *buffer_bytes.get_mut(sid).unwrap() = 0;
                }
                if round_all_millis_zero {
                    break;
                }
            } else if !round_had_records && stream_mode {
                for sid in &shard_ids {
                    let buf = buffers.get_mut(sid).unwrap();
                    let last_seq = last_seq_per_shard.get_mut(sid).unwrap();
                    if !buf.is_empty() {
                        self.flush_shard_buffer(&stream_name, sid, buf, last_seq, ctx.as_ref())?;
                    }
                    *buffer_bytes.get_mut(sid).unwrap() = 0;
                }
                if round_all_millis_zero {
                    sleep(Duration::from_millis(backoff_ms)).await;
                    backoff_ms = (backoff_ms * 2).min(5000);
                } else {
                    backoff_ms = 100;
                }
            } else {
                backoff_ms = 100;
            }
        }

        for sid in &shard_ids {
            let buf = buffers.get_mut(sid).unwrap();
            let last_seq = last_seq_per_shard.get_mut(sid).unwrap();
            if !buf.is_empty() {
                self.flush_shard_buffer(&stream_name, sid, buf, last_seq, ctx.as_ref())?;
            }
        }

        info!("Kinesis input plugin sync complete");
        Ok(())
    }
}

#[async_trait]
impl DataSource for DataSourceKinesisPlugin {
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

#[cfg(test)]
mod tests {
    use super::*;
    use skippr_runtime_sdk::plugins::cdc::CheckpointEnvelope;
    use skippr_runtime_sdk::plugins::{OffsetValidationEntry, SourcePayloadTask};
    use skippr_runtime_sdk::protocol::RuntimeOffsetMaterializationHint;
    use skippr_runtime_sdk::source_compat::ThroughputMetrics;
    use std::sync::Mutex;

    struct MemoryCheckpointCtx {
        checkpoints: Mutex<HashMap<String, CheckpointEnvelope>>,
    }

    impl SourceSyncContext for MemoryCheckpointCtx {
        fn submit_payload_tasks(
            &self,
            _tasks: Vec<SourcePayloadTask>,
        ) -> Result<ThroughputMetrics, std::io::Error> {
            Ok(ThroughputMetrics {
                bytes_per_second: 0,
                active_cores: 0,
                queue_length: 0,
                optimal_chunk_size: 0,
            })
        }

        fn validate_offset_batch(
            &self,
            entries: &[OffsetValidationEntry],
        ) -> Result<Vec<bool>, std::io::Error> {
            Ok(vec![false; entries.len()])
        }

        fn relay_offset_hints(
            &self,
            _hints: Vec<RuntimeOffsetMaterializationHint>,
        ) -> Result<(), std::io::Error> {
            Ok(())
        }

        fn store_checkpoint(&self, key: &str, envelope: &CheckpointEnvelope) -> Result<(), String> {
            self.checkpoints
                .lock()
                .expect("checkpoint map")
                .insert(key.to_string(), envelope.clone());
            Ok(())
        }

        fn load_checkpoint_envelope(
            &self,
            key: &str,
        ) -> Result<Option<CheckpointEnvelope>, String> {
            Ok(self
                .checkpoints
                .lock()
                .expect("checkpoint map")
                .get(key)
                .cloned())
        }
    }

    #[test]
    fn checkpoint_key_is_host_store_not_seq_file() {
        assert_eq!(
            DataSourceKinesisPlugin::checkpoint_key("orders", "shardId-000000000001"),
            "kinesis:orders:shardId-000000000001"
        );
    }

    #[test]
    fn shard_seq_round_trips_through_host_checkpoint() {
        let ctx = MemoryCheckpointCtx {
            checkpoints: Mutex::new(HashMap::new()),
        };
        DataSourceKinesisPlugin::write_checkpoint(
            &ctx,
            "orders",
            "shardId-000000000001",
            "4960000000000000000001",
        )
        .expect("store");
        assert_eq!(
            DataSourceKinesisPlugin::read_checkpoint(&ctx, "orders", "shardId-000000000001")
                .as_deref(),
            Some("4960000000000000000001")
        );
        assert!(
            DataSourceKinesisPlugin::read_checkpoint(&ctx, "orders", "shardId-000000000002")
                .is_none()
        );
    }
}
