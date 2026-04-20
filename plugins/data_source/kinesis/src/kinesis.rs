use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_sdk_kinesis::types::ShardIteratorType;
use aws_sdk_kinesis::Client;
use serde_derive::Deserialize;
use tokio::time::{sleep, Duration};
use tracing::{error, info};

use crate::helpers::configuration::Config;
use crate::helpers::offsets::{OffsetKey, Offsets};
use crate::helpers::plugin_config::PluginConfigEntry;
use crate::ingest_work::{Ingest, IngestBatch, IngestTask, IngestTasks};
use crate::plugins::{DataSink, DataSource};
use crate::RUNNING;

#[derive(Debug, Deserialize, Clone)]
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
    pub(crate) ingest: Ingest,
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
    fn checkpoint_path(stream: &str, shard_id: &str) -> PathBuf {
        let safe_stream = stream.replace(['/', '\\'], "_");
        let safe_shard = shard_id.replace(['/', '\\'], "_");
        PathBuf::from(Config::get_data_dir())
            .join("kinesis_checkpoints")
            .join(safe_stream)
            .join(format!("{safe_shard}.seq"))
    }

    async fn read_checkpoint(stream: &str, shard_id: &str) -> Option<String> {
        let p = Self::checkpoint_path(stream, shard_id);
        tokio::fs::read_to_string(p)
            .await
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    async fn write_checkpoint(stream: &str, shard_id: &str, seq: &str) -> std::io::Result<()> {
        let p = Self::checkpoint_path(stream, shard_id);
        if let Some(parent) = p.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(p, seq.as_bytes()).await?;
        Ok(())
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

        DataSourceKinesisPlugin {
            ingest: Ingest::new(),
            config,
            client,
        }
    }

    pub async fn new() -> Self {
        let config: DataSourceKinesisPluginConfig = match Config::get_pipeline_input_plugin_config()
        {
            Ok(input_config) => input_config.try_into().unwrap_or_else(|e| panic!("{}", e)),
            Err(_) => DataSourceKinesisPluginConfig {
                stream_name: Config::getenv("KINESIS_STREAM_NAME", ""),
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

        Self::from_config(config).await
    }

    pub async fn with_runtime_config(config: DataSourceKinesisPluginConfig) -> Self {
        Self::from_config(config).await
    }

    fn flush_shard_buffer(
        &mut self,
        shard_id: &str,
        pending: &mut Vec<IngestBatch>,
        last_seq: &mut Option<String>,
        offsets: &Arc<Offsets>,
        output: &Arc<Box<dyn DataSink + Send + Sync>>,
    ) {
        if pending.is_empty() {
            return;
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
        let mut tasks = IngestTasks::new();
        tasks.add(IngestTask::new(batch, offsets.clone(), output.clone()));
        self.ingest
            .ingest_file(&Arc::new(tasks), offsets, output.clone());
        let stream = self.config.stream_name.clone();
        let sid = shard_id.to_string();
        let seq = last_seq.clone().unwrap_or_default();
        if !seq.is_empty() {
            let h = tokio::runtime::Handle::try_current();
            if let Ok(handle) = h {
                let _ = handle.block_on(Self::write_checkpoint(&stream, &sid, &seq));
            }
        }
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

    pub async fn sync(
        &mut self,
        offsets: Arc<Offsets>,
        shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
    ) {
        let stream_name = self.config.stream_name.clone();
        if stream_name.is_empty() {
            error!("Kinesis stream_name is empty");
            return;
        }

        let stream_mode = self.config.mode.as_deref().unwrap_or("batch") == "stream";
        let batch_limit = self.config.batch_size_bytes.unwrap_or(1_024_000).max(1) as usize;
        let ns_display = format!("kinesis.{}", stream_name);

        let shard_ids = match self.list_all_shards().await {
            Ok(s) => s,
            Err(e) => {
                error!("Kinesis list_shards failed: {}", e);
                return;
            }
        };

        if shard_ids.is_empty() {
            info!("Kinesis stream {} has no shards", stream_name);
            return;
        }

        let mut iterators: HashMap<String, String> = HashMap::new();
        for sid in &shard_ids {
            let ckpt = Self::read_checkpoint(&stream_name, sid).await;
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
                        let ckpt = Self::read_checkpoint(&stream_name, sid).await;
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
                    let data = String::from_utf8_lossy(rec.data().as_ref()).into_owned();
                    let bytes = data.len();
                    let offset_key = OffsetKey {
                        namespace: format!("kinesis:{stream_name}:{sid}"),
                        partition: seq.clone(),
                    };
                    buf.push(IngestBatch {
                        offset_key,
                        data,
                        bytes,
                        source_uri: format!("kinesis://{stream_name}/{sid}"),
                        namespace: Some(ns_display.clone()),
                        cdc_rows: None,
                    });
                    *bbytes += bytes;
                    *last_seq = Some(seq);

                    if *bbytes >= batch_limit {
                        self.flush_shard_buffer(sid, buf, last_seq, &offsets, &shared_output);
                        *bbytes = 0;
                    }
                }
            }

            if !round_had_records && !stream_mode {
                for sid in &shard_ids {
                    let buf = buffers.get_mut(sid).unwrap();
                    let last_seq = last_seq_per_shard.get_mut(sid).unwrap();
                    if !buf.is_empty() {
                        self.flush_shard_buffer(sid, buf, last_seq, &offsets, &shared_output);
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
                        self.flush_shard_buffer(sid, buf, last_seq, &offsets, &shared_output);
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
                self.flush_shard_buffer(sid, buf, last_seq, &offsets, &shared_output);
            }
        }

        info!("Kinesis input plugin sync complete");
    }
}

#[async_trait]
impl DataSource for DataSourceKinesisPlugin {
    async fn sync(
        &mut self,
        offsets: Arc<Offsets>,
        output: Arc<Box<dyn DataSink + Send + Sync>>,
    ) -> Result<(), std::io::Error> {
        self.sync(offsets, output).await;
        Ok(())
    }
}
