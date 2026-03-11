use crate::helpers::configuration::{Config, InputPluginConfig};

use aws_sdk_s3::Client;

use flate2::read::GzDecoder;

use std::io::{Cursor, Read};

use std::sync::Arc;

use aws_sdk_s3::operation::get_object::{GetObjectError, GetObjectOutput};

use std::fs;
use std::time::Duration;
// use aws_sdk_s3::types::Object;
// use futures::future::join_all;

use serde_derive::Deserialize;

use crate::helpers::offsets::{OffsetKey, OffsetTypes, Offsets};
use crate::ingest_work::{Ingest, IngestBatch, IngestTask, IngestTasks, ThroughputMetrics};

use tokio::sync::Semaphore;
// use crate::helpers::timed_rwlock::TimedRwLock;
use crate::helpers::Helpers;
use crate::plugins::DataOutputPlugin;
use futures::stream::{self, StreamExt};
use std::io::BufRead as _;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering as AtomicOrdering;
use tokio::sync::OwnedSemaphorePermit;
use tracing::{error, info};

fn read_meminfo_kib(key: &str) -> Option<u64> {
    if let Ok(file) = std::fs::File::open("/proc/meminfo") {
        let reader = std::io::BufReader::new(file);
        for line in reader.lines().flatten() {
            if line.starts_with(key) {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    if let Ok(v) = parts[1].parse::<u64>() {
                        return Some(v);
                    }
                }
            }
        }
    }
    None
}

fn read_mem_total_mib() -> Option<u64> {
    read_meminfo_kib("MemTotal:").map(|kib| kib / 1024)
}
fn read_mem_available_mib() -> Option<u64> {
    read_meminfo_kib("MemAvailable:").map(|kib| kib / 1024)
}

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

impl From<InputPluginConfig> for DataSourceS3PluginConfig {
    fn from(plugin_config: InputPluginConfig) -> Self {
        match plugin_config {
            InputPluginConfig::S3(s3_config) => s3_config,
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
    optimal_chunk_size: usize,
    #[allow(dead_code)]
    pending_cap: Option<Arc<AtomicUsize>>,
}

impl DataSourceS3Plugin {
    /// Create a new S3 input plugin
    pub async fn new() -> DataSourceS3Plugin {
        let s3_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .load()
            .await;

        let data_dir = Config::get_data_dir();
        let temp_dir = &format!("{}/source_buffer", data_dir);

        match fs::create_dir(temp_dir) {
            Ok(_g) => {}
            Err(_err) => {}
        }

        let s3_client = Client::new(&s3_config);

        let config: DataSourceS3PluginConfig = match Config::get_pipeline_input_plugin_config() {
            Ok(config) => config.into(),
            Err(_) => DataSourceS3PluginConfig {
                format: None,
                batch_size_seconds: Some(
                    Config::getenv("DATA_SOURCE_BATCH_SIZE_SECONDS", "600")
                        .parse::<i64>()
                        .unwrap(),
                ),
                batch_size_bytes: Some(
                    Config::getenv("DATA_SOURCE_BATCH_SIZE_BYTES", "1024000")
                        .parse::<i64>()
                        .unwrap(),
                ),
                s3_bucket: Config::getenv("DATA_SOURCE_S3_BUCKET", ""),
                s3_prefix: Config::getenv("DATA_SOURCE_S3_PREFIX", ""),
                s3_prefix_ordered_depth: Some(
                    Config::getenv("DATA_SOURCE_S3_PREFIX_ORDERED_DEPTH", "0")
                        .parse::<usize>()
                        .unwrap(),
                ),
                s3_delimiter: Some(Config::getenv("DATA_SOURCE_S3_DELIMITER", "/")),
            },
        };

        DataSourceS3Plugin {
            s3_client,
            ingest: Ingest::new(),
            config,
            temp_dir: temp_dir.to_string(),
            prefixes: Vec::new(),
            active_threads: 0,
            optimal_chunk_size: 0,
            pending_cap: None,
        }
    }

    // Continuation token persistence removed; rely on paginator state.

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
        let s3_bucket_outer = s3_bucket_dl.clone();
        let s3_bucket_ns = s3_bucket.clone();
        let delimiter = "/".to_string();
        let inventory_prefix = self.config.s3_prefix.clone();
        let total_cpus = num_cpus::get();

        info!(
            "Syncing bucket: {}, prefix: {}",
            s3_bucket, inventory_prefix
        );

        let chunk_size = self.config.batch_size_bytes.unwrap_or(10_000_000) as usize;
        self.optimal_chunk_size = chunk_size;

        let mut s3_prefix = inventory_prefix.trim_start_matches(&delimiter).to_string();
        if s3_prefix == delimiter || s3_prefix == format!(".{}", delimiter) {
            s3_prefix = "".to_string();
        }

        // Cap S3 download memory on CI by default; allow env override
        let is_ci = {
            let ga = Config::getenv("GITHUB_ACTIONS", "");
            let ci = Config::getenv("CI", "");
            ga.eq_ignore_ascii_case("true") || ci == "1" || ci.eq_ignore_ascii_case("true")
        };
        let default_mb = if is_ci { "2048" } else { "4096" };
        let parsed_mb = Config::getenv("S3_DOWNLOAD_MEMORY_MB", default_mb)
            .parse::<u32>()
            .unwrap_or_else(|_| if is_ci { 2048 } else { 4096 });
        // Clamp to a reasonable range to avoid runaway allocations
        let mem_budget_mb: u32 = parsed_mb.clamp(256, if is_ci { 2048 } else { 16384 });
        // Expected decompression expansion factor (used per-object, not for total capacity)
        let inflate_ratio_env = Config::getenv("S3_INFLATE_RATIO", "4");
        let inflate_ratio: u32 = inflate_ratio_env.parse::<u32>().unwrap_or(4).clamp(1, 32);
        let mem_sem = Arc::new(Semaphore::new(mem_budget_mb as usize));

        info!(
            "Starting stream pipeline (cpus={}, dl_mem={} MiB, init_chunk={})",
            total_cpus,
            mem_budget_mb,
            Helpers::human_readable_size(chunk_size as u64)
        );

        let offsets_clone = offsets.clone();
        // Build keys iterator by pulling pages manually (compatible with SDK stream type)
        let pager = self
            .s3_client
            .list_objects_v2()
            .bucket(s3_bucket.clone())
            .prefix(s3_prefix.clone())
            .into_paginator()
            .page_size(1000)
            .send();

        // Stream keys directly from the paginator without materializing all results
        let keys_stream = stream::unfold(pager, |mut p| async move {
            match p.next().await {
                Some(Ok(page)) => {
                    let items: Vec<(String, i64)> = page
                        .contents
                        .unwrap_or_default()
                        .into_iter()
                        .filter_map(|obj| {
                            obj.key()
                                .map(|k| (k.to_string(), obj.size().unwrap_or_default()))
                        })
                        .collect();
                    Some((stream::iter(items), p))
                }
                _ => None,
            }
        })
        .flatten()
        .filter(move |(key, _size)| {
            let offsets = offsets_clone.clone();
            let ns = s3_bucket_filter.clone();
            let key_clone = key.clone();
            async move {
                let offset_key = OffsetKey {
                    namespace: ns,
                    partition: key_clone,
                };
                Some(true) != offsets.validate(&offset_key, OffsetTypes::Closed, 1)
            }
        });

        let dl_concurrency_env = Config::getenv("S3_DOWNLOAD_CONCURRENCY", "");
        let env_dl = dl_concurrency_env.parse::<usize>().ok().filter(|v| *v > 0);
        let tuned_dl = crate::metrics::counters::S3_DOWNLOAD_CONCURRENCY_TARGET
            .load(std::sync::atomic::Ordering::Relaxed);
        let dl_concurrency = env_dl.unwrap_or_else(|| tuned_dl.clamp(8, 512));
        if Config::log_wal_enabled() {
            info!(
                "tune: s3_download_concurrency={} (env_override={:?})",
                dl_concurrency, env_dl
            );
        }
        let dl_sem = Arc::new(Semaphore::new(dl_concurrency));

        // Background manager to dynamically adjust effective download concurrency to tuned target
        {
            let dl_sem_mgr = dl_sem.clone();
            tokio::spawn(async move {
                use std::sync::atomic::Ordering as AtomicOrdering;
                use tokio::time::{sleep, Duration};
                let mut configured_total: usize = dl_concurrency;
                let mut held: Vec<OwnedSemaphorePermit> = Vec::new();
                loop {
                    sleep(Duration::from_millis(500)).await;
                    let target = crate::metrics::counters::S3_DOWNLOAD_CONCURRENCY_TARGET
                        .load(AtomicOrdering::Relaxed)
                        .clamp(8, 512);
                    if target > configured_total {
                        let add = target - configured_total;
                        dl_sem_mgr.add_permits(add);
                        configured_total = target;
                        // Release any held permits up to new target
                        while held.len() > 0
                            && held.len() + dl_sem_mgr.available_permits() > 0
                            && held.len()
                                > (configured_total.saturating_sub(dl_sem_mgr.available_permits()))
                        {
                            // drop one permit
                            let _ = held.pop();
                        }
                    } else if target < configured_total {
                        let mut need_to_hold = configured_total - target;
                        // Try to acquire and hold permits to reduce effective concurrency
                        while need_to_hold > 0 {
                            match dl_sem_mgr.clone().try_acquire_owned() {
                                Ok(p) => {
                                    held.push(p);
                                    need_to_hold -= 1;
                                }
                                Err(_) => break,
                            }
                        }
                        configured_total = target;
                    }
                }
            });
        }
        // Background memory manager to dynamically adjust effective download memory permits and staging cap
        {
            let mem_sem_mgr = mem_sem.clone();
            let pending_default_is_ci = {
                let ga = Config::getenv("GITHUB_ACTIONS", "");
                let ci = Config::getenv("CI", "");
                ga.eq_ignore_ascii_case("true") || ci == "1" || ci.eq_ignore_ascii_case("true")
            };
            let pending_default: usize = Config::getenv("INGEST_PENDING_MAX_BYTES", "")
                .parse::<usize>()
                .ok()
                .filter(|v| *v > 0)
                .unwrap_or_else(|| {
                    if pending_default_is_ci {
                        512 * 1024 * 1024
                    } else {
                        2 * 1024 * 1024 * 1024
                    }
                });
            let pending_cap = Arc::new(AtomicUsize::new(pending_default));
            // Expose to outer scope
            let pending_cap_outer = pending_cap.clone();
            // Spawn manager
            tokio::spawn(async move {
                let mut configured_total: usize = mem_budget_mb as usize;
                let mut held: Vec<OwnedSemaphorePermit> = Vec::new();
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
                    // Read memory stats (Linux /proc). If unavailable, skip adjustments
                    let (avail_mib_opt, total_mib_opt) =
                        (read_mem_available_mib(), read_mem_total_mib());
                    if avail_mib_opt.is_none() || total_mib_opt.is_none() {
                        continue;
                    }
                    let avail_mib = avail_mib_opt.unwrap();
                    let total_mib = total_mib_opt.unwrap();
                    // Keep at least 10% free; target download memory to at most 50% of available
                    let min_free_mib = (total_mib as f64 * 0.1) as u64;
                    let target_mem_mib: usize = if avail_mib > min_free_mib {
                        ((avail_mib as f64) * 0.8) as usize
                    } else {
                        ((avail_mib as f64) * 0.3) as usize
                    };
                    let target_mem_mib = target_mem_mib.clamp(256, mem_budget_mb as usize);
                    // Adjust semaphore to target
                    if target_mem_mib > configured_total {
                        let add = target_mem_mib - configured_total;
                        mem_sem_mgr.add_permits(add);
                        configured_total = target_mem_mib;
                        // Release any held permits beyond need
                        while held.len() > 0
                            && held.len() + mem_sem_mgr.available_permits() > 0
                            && held.len()
                                > (configured_total.saturating_sub(mem_sem_mgr.available_permits()))
                        {
                            let _ = held.pop();
                        }
                    } else if target_mem_mib < configured_total {
                        let mut need_to_hold = configured_total - target_mem_mib;
                        while need_to_hold > 0 {
                            match mem_sem_mgr.clone().try_acquire_owned() {
                                Ok(p) => {
                                    held.push(p);
                                    need_to_hold -= 1;
                                }
                                Err(_) => break,
                            }
                        }
                        configured_total = target_mem_mib;
                    }
                    // Tune pending staging cap to at most 25% of available RAM, min 64 MiB
                    let pending_target = ((avail_mib as usize) * 1024 * 1024) / 4;
                    let pending_target = pending_target.clamp(64 * 1024 * 1024, pending_default);
                    pending_cap.store(pending_target, AtomicOrdering::Relaxed);
                }
            });
            // Store the atomic in self via closure capture: replace fixed pending cap below
            // We pass this arc down by capturing in the stream loop via move
            self.pending_cap = Some(pending_cap_outer);
        }
        let s3_client_clone = self.s3_client.clone();
        let mem_sem_clone = mem_sem.clone();
        let dl_sem_clone = dl_sem.clone();
        let inflate_ratio_clone = inflate_ratio;
        let download_stream = keys_stream
            .map(move |(key, size_bytes)| {
                let s3 = s3_client_clone.clone();
                let bucket = s3_bucket_dl.clone();
                let mem = mem_sem_clone.clone();
                let dl_ctrl = dl_sem_clone.clone();
                let inflate_ratio = inflate_ratio_clone;
                async move {
                    // Acquire permits for estimated decompressed MiB (min 1)
                    let est_uncompressed_bytes: u64 = (std::cmp::max(1i64, size_bytes) as u64)
                        .saturating_mul(inflate_ratio as u64);
                    let permits: u32 = ((est_uncompressed_bytes + 1_048_575) / 1_048_576) as u32;
                    // Retain memory permits for the lifetime of this task to avoid premature drop
                    let mut mem_permits: Vec<OwnedSemaphorePermit> = Vec::with_capacity(2);
                    if let Ok(p0) = mem.clone().acquire_many_owned(permits.max(1)).await {
                        mem_permits.push(p0);
                    }
                    let p_dl = dl_ctrl.acquire_owned().await.ok();
                    let res =
                        DataSourceS3Plugin::download_s3_object_with_backoff(&s3, &bucket, &key)
                            .await;
                    let out = match res {
                        Ok(response) => {
                            let mut data = response.body;
                            let mut data_vec = Vec::new();
                            while let Some(chunk) = data.next().await {
                                if let Ok(bytes) = chunk {
                                    data_vec.extend_from_slice(&bytes);
                                }
                            }
                            let is_gz = key.contains(".gz");
                            let decoded = if is_gz {
                                let res = tokio::task::spawn_blocking(move || {
                                    let c = Cursor::new(data_vec);
                                    let mut stream = GzDecoder::new(c);
                                    let mut decompressed = String::new();
                                    stream
                                        .read_to_string(&mut decompressed)
                                        .map(|_| decompressed)
                                })
                                .await;
                                match res {
                                    Ok(Ok(s)) => {
                                        // Top-up permits if actual size exceeded estimate
                                        let actual_bytes = s.len() as u64;
                                        if actual_bytes > est_uncompressed_bytes {
                                            let extra = ((actual_bytes - est_uncompressed_bytes)
                                                + 1_048_575)
                                                / 1_048_576;
                                            if extra > 0 {
                                                if let Ok(p1) = mem
                                                    .clone()
                                                    .acquire_many_owned(extra as u32)
                                                    .await
                                                {
                                                    // retain until function end
                                                    mem_permits.push(p1);
                                                }
                                            }
                                        }
                                        Some(s)
                                    }
                                    _ => None,
                                }
                            } else {
                                String::from_utf8(data_vec).ok()
                            };
                            decoded.map(|s| (key, s))
                        }
                        Err(_) => None,
                    };
                    if let Some(perm_dl) = p_dl {
                        drop(perm_dl);
                    }
                    out
                }
            })
            .buffer_unordered(dl_concurrency.min(32));

        let mut current_batch: Vec<IngestBatch> = Vec::new();
        let mut current_bytes: usize = 0;
        let mut pending_tasks: Vec<IngestTask> = Vec::with_capacity(total_cpus);
        // Bound total bytes staged in memory before dispatching to ingest threads
        // Use dynamic pending cap if memory manager is active; fallback to env default
        let is_ci_pending = {
            let ga = Config::getenv("GITHUB_ACTIONS", "");
            let ci = Config::getenv("CI", "");
            ga.eq_ignore_ascii_case("true") || ci == "1" || ci.eq_ignore_ascii_case("true")
        };
        let pending_max_bytes_env = Config::getenv("INGEST_PENDING_MAX_BYTES", "");
        let pending_default_max: usize = pending_max_bytes_env
            .parse::<usize>()
            .ok()
            .filter(|v| *v > 0)
            .unwrap_or_else(|| {
                if is_ci_pending {
                    512 * 1024 * 1024
                } else {
                    2 * 1024 * 1024 * 1024
                }
            });
        let pending_cap_arc = self
            .pending_cap
            .clone()
            .unwrap_or_else(|| Arc::new(AtomicUsize::new(pending_default_max)));
        let mut pending_bytes_sum: usize = 0;

        futures::pin_mut!(download_stream);
        while let Some(opt) = StreamExt::next(&mut download_stream).await {
            if let Some((key, str_data)) = opt {
                let bytes = str_data.len();
                current_bytes += bytes;
                let source_uri = format!("s3://{}/{}", s3_bucket_outer, key);
                current_batch.push(IngestBatch {
                    offset_key: OffsetKey {
                        namespace: s3_bucket_ns.clone(),
                        partition: key,
                    },
                    data: str_data,
                    bytes,
                    source_uri,
                });
                if current_bytes >= self.optimal_chunk_size {
                    let batch_bytes = current_bytes;
                    let batch = std::mem::take(&mut current_batch);
                    current_bytes = 0;
                    pending_bytes_sum = pending_bytes_sum.saturating_add(batch_bytes);
                    pending_tasks.push(IngestTask::new(
                        batch,
                        offsets.clone(),
                        shared_output.clone(),
                    ));
                    if pending_tasks.len() >= total_cpus
                        || pending_bytes_sum >= pending_cap_arc.load(AtomicOrdering::Relaxed)
                    {
                        let mut tasks = IngestTasks::new();
                        for t in pending_tasks.drain(..) {
                            tasks.add(t);
                        }
                        let tasks_arc = Arc::new(tasks);
                        let metrics =
                            self.ingest
                                .ingest_file(&tasks_arc, &offsets, shared_output.clone());
                        self.active_threads = metrics.active_cores;
                        self.optimal_chunk_size = metrics.optimal_chunk_size;
                        pending_bytes_sum = 0;
                    }
                }
            }
        }

        if !current_batch.is_empty() {
            let batch_bytes = current_bytes;
            let _ = pending_bytes_sum.saturating_add(batch_bytes);
            pending_tasks.push(IngestTask::new(
                std::mem::take(&mut current_batch),
                offsets.clone(),
                shared_output.clone(),
            ));
        }
        if !pending_tasks.is_empty() {
            let mut tasks = IngestTasks::new();
            for t in pending_tasks.drain(..) {
                tasks.add(t);
            }
            let tasks_arc = Arc::new(tasks);
            let _ = self
                .ingest
                .ingest_file(&tasks_arc, &offsets, shared_output.clone());
        }

        self.ingest.wait_for_completion();
        info!("S3 stream pipeline complete; ingest completed");
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

                    tokio::time::sleep(Duration::from_secs_f64(backoff_duration.as_secs_f64()))
                        .await;

                    if retries >= max_retries {
                        error!("Max retries reached for object {}", key);
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
    #[allow(dead_code)]
    async fn download_and_ingest(
        &mut self,
        _s3_bucket: &String,
        _keys: &Vec<Vec<String>>,
        _offsets: &Arc<Offsets>,
        _shared_output: Arc<Box<dyn DataOutputPlugin + Send + Sync>>,
        _chunk_size_current: i64,
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
