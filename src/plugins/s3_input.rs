use crate::helpers::configuration::{Config, PluginConfig};

use aws_sdk_s3::Client;

use flate2::read::GzDecoder;

use std::io::{Cursor, Read, Write};

use std::sync::{Arc};

use aws_sdk_s3::operation::get_object::{GetObjectError, GetObjectOutput};

use std::time::{Duration, Instant};
use std::{fs};
// use aws_sdk_s3::types::Object;
// use futures::future::join_all;

use serde_derive::Deserialize;
use once_cell::sync::Lazy;

use crate::helpers::offsets::{OffsetKey, OffsetTypes, Offsets};
use crate::ingest_work::{Ingest, IngestBatch, IngestTask, IngestTasks, ThroughputMetrics};

use tokio::sync::Semaphore;
// use crate::helpers::timed_rwlock::TimedRwLock;
use crate::plugins::DataOutputPlugin;
use crate::helpers::Helpers;
use tokio::sync::mpsc;
use tokio::sync::OwnedSemaphorePermit;
use tokio::task::JoinSet;
use futures::stream::{self, StreamExt};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Path for storing the S3 continuation token so we can resume syncs
const CONTINUATION_TOKEN_FILE: Lazy<String> = Lazy::new(|| {
    format!("{}/s3_input_continuation_token", Config::get_data_dir())
});

#[derive(Deserialize, Debug, Clone)]
pub struct DataSourceS3PluginConfig {
    pub format: Option<String>,
    pub batch_size_seconds: Option<i64>,
    pub batch_size_bytes: Option<i64>,

    pub s3_bucket: String,
    pub s3_prefix: String,
    #[allow(dead_code)]
    pub s3_prefix_ordered_depth: Option<usize>,
    #[allow(dead_code)]
    pub s3_delimiter: Option<String>,
}

impl From<PluginConfig> for DataSourceS3PluginConfig {
    fn from(plugin_config: PluginConfig) -> Self {
        match plugin_config {
            PluginConfig::S3(s3_config) => s3_config,
            _ => panic!("Invalid plugin type"),
        }
    }
}

/// Plugin for ingesting data from Amazon S3
pub struct DataSourceS3Plugin {
    s3_client: Client,
    ingest: Ingest,
    config: DataSourceS3PluginConfig,
    #[allow(dead_code)]
    temp_dir: String,
    #[allow(dead_code)]
    prefixes: Vec<(String, usize)>,
    active_threads: usize,
    optimal_chunk_size: usize
}

impl DataSourceS3Plugin {
    /// Create a new S3 input plugin
    pub async fn new() -> DataSourceS3Plugin {
        let s3_config = aws_config::defaults(aws_config::BehaviorVersion::latest()).load().await;

        let data_dir = Config::get_data_dir();
        let temp_dir = &format!("{}/source_buffer", data_dir);

        match fs::create_dir(temp_dir) {
            Ok(_g) => {}
            Err(_err) => {}
        }

        let s3_client = Client::new(&s3_config);

        let config: DataSourceS3PluginConfig = match Config::get_pipline_plugin_config("input") {
            Ok(config) => config.into(),
            Err(_) => DataSourceS3PluginConfig {
                format: None,
                batch_size_seconds: Some(Config::getenv("DATA_SOURCE_BATCH_SIZE_SECONDS", "600").parse::<i64>().unwrap()),
                batch_size_bytes: Some(Config::getenv("DATA_SOURCE_BATCH_SIZE_BYTES", "1024000").parse::<i64>().unwrap()),
                s3_bucket: Config::getenv("DATA_SOURCE_S3_BUCKET", ""),
                s3_prefix: Config::getenv("DATA_SOURCE_S3_PREFIX", ""),
                s3_prefix_ordered_depth: Some(Config::getenv("DATA_SOURCE_S3_PREFIX_ORDERED_DEPTH", "0").parse::<usize>().unwrap()),
                s3_delimiter: Some(Config::getenv("DATA_SOURCE_S3_DELIMITER", "/")),
            }
        };

        DataSourceS3Plugin {
            s3_client,
            ingest: Ingest::new(),
            config,
            temp_dir: temp_dir.to_string(),
            prefixes: Vec::new(),
            active_threads: 0,
            optimal_chunk_size: 0,
        }
    }

    /// Save the continuation token to a file for resuming later
    fn save_continuation_token(token: &str) {
        if let Ok(mut file) = fs::File::create(&*CONTINUATION_TOKEN_FILE) {
            if let Err(e) = file.write_all(token.as_bytes()) {
                println!("Failed to save continuation token: {}", e);
            }
        }
    }

    /// Load the continuation token from a file
    fn load_continuation_token() -> Option<String> {
        match fs::read_to_string(&*CONTINUATION_TOKEN_FILE) {
            Ok(token) => Some(token.trim().to_string()),
            Err(_) => None,
        }
    }

    /// Synchronize data from S3 bucket to the ingestion pipeline
    ///
    /// This method:
    /// 1. Lists objects from the configured S3 bucket and prefix
    /// 2. For each batch of objects, downloads and processes them
    /// 3. Uses continuation tokens to resume listing where it left off
    pub async fn sync(
        &mut self,
        offsets: Arc<Offsets>,
        shared_output: Arc<Box<dyn DataOutputPlugin + Send + Sync>>,
    ) {
        // Use new stream-based pipeline implementation
        self.sync_stream(offsets, shared_output).await;
    }

    async fn sync_stream(
        &mut self,
        offsets: Arc<Offsets>,
        shared_output: Arc<Box<dyn DataOutputPlugin + Send + Sync>>,
    ) {
        let s3_bucket = self.config.s3_bucket.clone();
        let s3_bucket_filter = s3_bucket.clone();
        let s3_bucket_dl = s3_bucket.clone();
        let s3_bucket_ns = s3_bucket.clone();
        let delimiter = "/".to_string();
        let inventory_prefix = self.config.s3_prefix.clone();
        let total_cpus = num_cpus::get();

        println!("Syncing bucket: {}, prefix: {}", s3_bucket, inventory_prefix);

        let chunk_size = self.config.batch_size_bytes.unwrap_or(10_000_000) as usize;
        self.optimal_chunk_size = chunk_size;

        let mut s3_prefix = inventory_prefix.trim_start_matches(&delimiter).to_string();
        if s3_prefix == delimiter || s3_prefix == format!(".{}", delimiter) { s3_prefix = "".to_string(); }

        let mem_budget_mb: u32 = Config::getenv("S3_DOWNLOAD_MEMORY_MB", "4096").parse::<u32>().unwrap_or(4096);
        let mem_sem = Arc::new(Semaphore::new(mem_budget_mb as usize));

        println!("Starting stream pipeline (cpus={}, dl_mem={} MiB, init_chunk={})", total_cpus, mem_budget_mb, Helpers::human_readable_size(chunk_size as u64));

        let offsets_clone = offsets.clone();
        // Build keys iterator by pulling pages manually (compatible with SDK stream type)
        let mut pager = self.s3_client
            .list_objects_v2()
            .bucket(s3_bucket.clone())
            .prefix(s3_prefix.clone())
            .into_paginator()
            .page_size(1000)
            .send();

        // Collect keys lazily into a Vec to drive the rest of the stream
        let mut all_keys: Vec<(String, i64)> = Vec::new();
        while let Some(page_res) = pager.next().await {
            if let Ok(page) = page_res {
                if let Some(objects) = page.contents {
                    for obj in objects {
                        if let Some(k) = obj.key() { all_keys.push((k.to_string(), obj.size().unwrap_or_default())); }
                    }
                }
            }
        }

        let keys_stream = stream::iter(all_keys.into_iter())
            .filter(move |(key, _size)| {
                let offsets = offsets_clone.clone();
                let ns = s3_bucket_filter.clone();
                let key_clone = key.clone();
                async move {
                    let offset_key = OffsetKey { namespace: ns, partition: key_clone };
                    Some(true) != offsets.validate(&offset_key, OffsetTypes::Closed, 1)
                }
            });

        let dl_concurrency_env = Config::getenv("S3_DOWNLOAD_CONCURRENCY", "");
        let env_dl = dl_concurrency_env.parse::<usize>().ok().filter(|v| *v > 0);
        let tuned_dl = crate::metrics::counters::S3_DOWNLOAD_CONCURRENCY_TARGET.load(std::sync::atomic::Ordering::Relaxed);
        let dl_concurrency = env_dl.unwrap_or_else(|| tuned_dl.clamp(8, 512));
        println!("tune: s3_download_concurrency={} (env_override={:?})", dl_concurrency, env_dl);
        let s3_client_clone = self.s3_client.clone();
        let mem_sem_clone = mem_sem.clone();
        let mut download_stream = keys_stream.map(move |(key, size_bytes)| {
            let s3 = s3_client_clone.clone();
            let bucket = s3_bucket_dl.clone();
            let mem = mem_sem_clone.clone();
            async move {
                // Acquire permits roughly equal to MiB of object size (min 1)
                let permits: u32 = (((std::cmp::max(1i64, size_bytes) as u64) + 1_048_575) / 1_048_576) as u32;
                let p = mem.acquire_many_owned(permits).await.ok();
                let res = DataSourceS3Plugin::download_s3_object_with_backoff(&s3, &bucket, &key).await;
                let out = match res {
                    Ok(response) => {
                        let mut data = response.body;
                        let mut data_vec = Vec::new();
                        while let Some(chunk) = data.next().await { if let Ok(bytes) = chunk { data_vec.extend_from_slice(&bytes); } }
                        let is_gz = key.contains(".gz");
                        let decoded = if is_gz {
                            let res = tokio::task::spawn_blocking(move || {
                                let c = Cursor::new(data_vec);
                                let mut stream = GzDecoder::new(c);
                                let mut decompressed = String::new();
                                stream.read_to_string(&mut decompressed).map(|_| decompressed)
                            }).await;
                            match res { Ok(Ok(s)) => Some(s), _ => None }
                    } else {
                            String::from_utf8(data_vec).ok()
                        };
                        decoded.map(|s| (key, s))
                    }
                    Err(_) => None,
                };
                if let Some(perm) = p { drop(perm); }
                out
            }
        }).buffer_unordered(dl_concurrency);

        let mut current_batch: Vec<IngestBatch> = Vec::new();
        let mut current_bytes: usize = 0;
        let mut pending_tasks: Vec<IngestTask> = Vec::with_capacity(total_cpus);

        futures::pin_mut!(download_stream);
        while let Some(opt) = StreamExt::next(&mut download_stream).await {
            if let Some((key, str_data)) = opt {
                let bytes = str_data.len();
                current_bytes += bytes;
                current_batch.push(IngestBatch { offset_key: OffsetKey { namespace: s3_bucket_ns.clone(), partition: key }, data: str_data, bytes });
                if current_bytes >= self.optimal_chunk_size {
                    let batch = std::mem::take(&mut current_batch);
                    current_bytes = 0;
                    pending_tasks.push(IngestTask::new(batch, offsets.clone(), shared_output.clone()));
                    if pending_tasks.len() >= total_cpus {
                        let mut tasks = IngestTasks::new();
                        for t in pending_tasks.drain(..) { tasks.add(t); }
                        let tasks_arc = Arc::new(tasks);
                        let metrics = self.ingest.ingest_file(&tasks_arc, &offsets, shared_output.clone());
                        self.active_threads = metrics.active_cores;
                        self.optimal_chunk_size = metrics.optimal_chunk_size;
                    }
                }
            }
        }

        if !current_batch.is_empty() { pending_tasks.push(IngestTask::new(std::mem::take(&mut current_batch), offsets.clone(), shared_output.clone())); }
        if !pending_tasks.is_empty() {
            let mut tasks = IngestTasks::new();
            for t in pending_tasks.drain(..) { tasks.add(t); }
            let tasks_arc = Arc::new(tasks);
            let _ = self.ingest.ingest_file(&tasks_arc, &offsets, shared_output.clone());
        }

        self.ingest.wait_for_completion();
        println!("S3 stream pipeline complete; ingest completed");
    }

    /// Download an S3 object with exponential backoff retry logic
    async fn download_s3_object_with_backoff(
        s3_client: &Client,
        bucket: &String,
        key: &String,
    ) -> Result<GetObjectOutput, GetObjectError> {
        let mut retries = 0;
        let max_retries = 5;
        let mut backoff_duration = Duration::from_millis(1000);

        loop {
            let get_request = s3_client
                .get_object()
                .bucket(bucket.clone())
                .key(urldecode::decode(key.to_string()));

            match get_request.send().await {
                Ok(result) => {
                    return Ok(result);
                }
                Err(err) => {
                    retries += 1;
                    backoff_duration *= 2;
                    
                    tokio::time::sleep(Duration::from_secs_f64(backoff_duration.as_secs_f64())).await;

                    if retries >= max_retries {
                        println!("Max retries reached for object {}", key);
                        return Err(err.into_service_error());
                    }
                }
            }
        }
    }

    /// Download and ingest a batch of S3 objects
    ///
    /// This method:
    /// 1. Concurrently downloads all objects in the batch
    /// 2. Processes downloaded objects in batches by file type (gzip vs regular)
    /// 3. Submits the processed data to the ingestion pipeline
    /// 
    /// Returns: Throughput metrics that can be used to adjust future batch sizes
    async fn download_and_ingest(
        &mut self,
        s3_bucket: &String,
        keys: &Vec<Vec<String>>,
        offsets: &Arc<Offsets>,
        shared_output: Arc<Box<dyn DataOutputPlugin + Send + Sync>>,
        _chunk_size_current: i64
    ) -> ThroughputMetrics {
        // Deprecated in bounded pipeline path; keep a no-op metrics return for compatibility
        ThroughputMetrics {
                bytes_per_second: 0,
                active_cores: self.active_threads,
                queue_length: 0,
                optimal_chunk_size: self.optimal_chunk_size,
        }
    }
}

/// Helper struct to track a downloaded S3 object
struct Download {
    key: String,
    response: GetObjectOutput,
}


// Items used in new bounded pipeline
struct DownloadedItem {
    key: String,
    data: Vec<u8>,
    is_gz: bool,
    permit: OwnedSemaphorePermit,
}

