use crate::helpers::plugin_config::PluginConfigEntry;
use async_trait::async_trait;

use aws_sdk_s3::Client;

use flate2::read::GzDecoder;

use std::io::{Cursor, Read};

use std::sync::Arc;

use std::fs;
use std::time::Duration;

use serde_derive::Deserialize;

use skippr_runtime_sdk::progress::{OffsetKey, OffsetTypes};
use skippr_runtime_sdk::source_compat::{IngestBatch, SourcePayloadTask, SourceSyncContext};
use skippr_runtime_sdk::source_sync::offset_validation_entry;

use crate::helpers::Helpers;
use futures::stream::{self, StreamExt};
use skippr_runtime_sdk::plugins::DataSource;
use skippr_runtime_sdk::protocol::RuntimeExecutionContext;
use std::io::BufRead as _;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering as AtomicOrdering;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tracing::{error, info, warn};

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

fn env_string(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn env_truthy(name: &str) -> bool {
    env_string(name).is_some_and(|value| {
        value == "1"
            || value.eq_ignore_ascii_case("true")
            || value.eq_ignore_ascii_case("yes")
            || value.eq_ignore_ascii_case("on")
    })
}

fn runtime_is_ci() -> bool {
    env_truthy("GITHUB_ACTIONS") || env_truthy("CI")
}

fn runtime_log_wal_enabled() -> bool {
    env_truthy("LOG_WAL") || env_truthy("LOG_WAL_DEBUG") || env_truthy("LOG_WAL_UPLOADS")
}

#[derive(Deserialize, Debug, Clone)]
pub struct DataSourceS3PluginConfig {
    pub format: Option<String>,
    pub batch_size_seconds: Option<i64>,
    pub batch_size_bytes: Option<i64>,
    pub endpoint_url: Option<String>,

    pub s3_bucket: String,
    pub s3_prefix: String,
    /// AWS region for this bucket (required when the default credential region differs, e.g. cross-account picnic sync).
    pub region: Option<String>,
    #[allow(dead_code)]
    pub s3_prefix_ordered_depth: Option<usize>,
    #[allow(dead_code)]
    pub s3_delimiter: Option<String>,
}

impl TryFrom<PluginConfigEntry> for DataSourceS3PluginConfig {
    type Error = String;

    fn try_from(plugin_config: PluginConfigEntry) -> Result<Self, Self::Error> {
        plugin_config.decode_for_plugin("S3")
    }
}

/// Plugin for ingesting data from Amazon S3
pub struct DataSourceS3Plugin {
    s3_client: Client,
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
    async fn from_config(config: DataSourceS3PluginConfig, temp_dir: String) -> DataSourceS3Plugin {
        let mut config_loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
        if let Some(region) = config.region.as_deref().filter(|r| !r.is_empty()) {
            config_loader = config_loader.region(aws_config::Region::new(region.to_string()));
        }
        let shared_config = config_loader.load().await;
        let mut s3_client_config = aws_sdk_s3::config::Builder::from(&shared_config);
        if let Some(ref endpoint_url) = config.endpoint_url {
            s3_client_config = s3_client_config
                .endpoint_url(endpoint_url)
                .force_path_style(true);
        }

        DataSourceS3Plugin {
            s3_client: Client::from_conf(s3_client_config.build()),
            config,
            temp_dir,
            prefixes: Vec::new(),
            active_threads: 0,
            optimal_chunk_size: 0,
            pending_cap: None,
        }
    }

    pub async fn with_runtime_config(
        config: DataSourceS3PluginConfig,
        context: RuntimeExecutionContext,
    ) -> DataSourceS3Plugin {
        let runtime_child_data_dir = format!("{}/runtime_source_children/s3", context.data_dir);
        let temp_dir = format!("{}/source_buffer", runtime_child_data_dir);
        let _ = fs::create_dir_all(&temp_dir);
        Self::from_config(config, temp_dir).await
    }

    // Continuation token persistence removed; rely on paginator state.

    /// Synchronize data from S3 bucket to the ingestion pipeline
    ///
    /// This method:
    /// 1. Lists objects from the configured S3 bucket and prefix
    /// 2. For each batch of objects, downloads and processes them
    /// 3. Uses continuation tokens to resume listing where it left off
    pub async fn sync(&mut self, ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error> {
        self.sync_stream(ctx).await
    }

    async fn sync_stream(&mut self, ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error> {
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
        let is_ci = runtime_is_ci();
        let default_mb = if is_ci { "2048" } else { "4096" };
        let parsed_mb = env_string("S3_DOWNLOAD_MEMORY_MB")
            .unwrap_or_else(|| default_mb.to_string())
            .parse::<u32>()
            .unwrap_or_else(|_| if is_ci { 2048 } else { 4096 });
        // Clamp to a reasonable range to avoid runaway allocations
        let mem_budget_mb: u32 = parsed_mb.clamp(256, if is_ci { 2048 } else { 16384 });
        // Expected decompression expansion factor (used per-object, not for total capacity)
        let inflate_ratio_env = env_string("S3_INFLATE_RATIO").unwrap_or_else(|| "4".to_string());
        let inflate_ratio: u32 = inflate_ratio_env.parse::<u32>().unwrap_or(4).clamp(1, 32);
        let mem_sem = Arc::new(Semaphore::new(mem_budget_mb as usize));

        info!(
            "Starting stream pipeline (cpus={}, dl_mem={} MiB, init_chunk={})",
            total_cpus,
            mem_budget_mb,
            Helpers::human_readable_size(chunk_size as u64)
        );

        let mut pager = self
            .s3_client
            .list_objects_v2()
            .bucket(s3_bucket.clone())
            .prefix(s3_prefix.clone())
            .into_paginator()
            .page_size(1000)
            .send();

        let mut listed_keys = Vec::new();
        while let Some(page_result) = pager.next().await {
            let page = page_result.map_err(|err| {
                std::io::Error::other(format!("failed to list S3 objects: {err}"))
            })?;
            let items: Vec<(String, i64)> = page
                .contents
                .unwrap_or_default()
                .into_iter()
                .filter_map(|obj| {
                    obj.key()
                        .map(|k| (k.to_string(), obj.size().unwrap_or_default()))
                })
                .collect();
            if items.is_empty() {
                continue;
            }
            let validation_entries = items
                .iter()
                .map(|(key, _)| {
                    offset_validation_entry(
                        s3_bucket_filter.clone(),
                        key.clone(),
                        OffsetTypes::Closed,
                        1,
                    )
                })
                .collect::<Vec<_>>();
            let should_process = ctx.validate_offset_batch(&validation_entries)?;
            for ((key, size), process) in items.into_iter().zip(should_process) {
                if !process {
                    if runtime_log_wal_enabled() {
                        info!(
                            "s3 source: skipping closed object namespace={} key={}",
                            s3_bucket_filter, key
                        );
                    }
                    continue;
                }
                listed_keys.push((key, size));
            }
        }

        let keys_stream = stream::iter(listed_keys);

        let dl_concurrency_env = env_string("S3_DOWNLOAD_CONCURRENCY").unwrap_or_default();
        let env_dl = dl_concurrency_env.parse::<usize>().ok().filter(|v| *v > 0);
        let tuned_dl = skippr_runtime_sdk::metrics::counters::S3_DOWNLOAD_CONCURRENCY_TARGET
            .load(std::sync::atomic::Ordering::Relaxed);
        let dl_concurrency = env_dl.unwrap_or_else(|| tuned_dl.clamp(8, 512));
        if runtime_log_wal_enabled() {
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
                    let target =
                        skippr_runtime_sdk::metrics::counters::S3_DOWNLOAD_CONCURRENCY_TARGET
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
            let pending_default_is_ci = runtime_is_ci();
            let pending_default: usize = env_string("INGEST_PENDING_MAX_BYTES")
                .unwrap_or_default()
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
                    let out = match DataSourceS3Plugin::download_s3_object_bytes_with_backoff(
                        &s3, &bucket, &key,
                    )
                    .await
                    {
                        Ok(data_vec) => {
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
                                        Ok(s)
                                    }
                                    Ok(Err(err)) => Err(std::io::Error::other(format!(
                                        "failed to decompress gzip S3 object '{}': {}",
                                        key, err
                                    ))),
                                    Err(err) => Err(std::io::Error::other(format!(
                                        "failed to join gzip decode task for S3 object '{}': {}",
                                        key, err
                                    ))),
                                }
                            } else {
                                String::from_utf8(data_vec).map_err(|err| {
                                    std::io::Error::other(format!(
                                        "failed to decode UTF-8 contents for S3 object '{}': {}",
                                        key, err
                                    ))
                                })
                            };
                            decoded.map(|s| Some((key, s)))
                        }
                        Err(err) => Err(err),
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
        let mut pending_tasks: Vec<SourcePayloadTask> = Vec::with_capacity(total_cpus);
        let mut accepted_submissions = Vec::new();
        // Bound total bytes staged in memory before dispatching to ingest threads
        // Use dynamic pending cap if memory manager is active; fallback to env default
        let is_ci_pending = runtime_is_ci();
        let pending_max_bytes_env = env_string("INGEST_PENDING_MAX_BYTES").unwrap_or_default();
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
        while let Some(result) = StreamExt::next(&mut download_stream).await {
            if let Some((key, str_data)) = result? {
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
                    offset_pos: None,
                    source_uri,
                    namespace: None,
                    cdc_rows: None,
                });
                if current_bytes >= self.optimal_chunk_size {
                    let batch_bytes = current_bytes;
                    let batch = std::mem::take(&mut current_batch);
                    if runtime_log_wal_enabled() {
                        let first_key = batch
                            .first()
                            .map(|item| item.offset_key.partition.as_str())
                            .unwrap_or("");
                        let last_key = batch
                            .last()
                            .map(|item| item.offset_key.partition.as_str())
                            .unwrap_or("");
                        info!(
                            "s3 source: queueing ingest task objects={} bytes={} first_key={} last_key={}",
                            batch.len(),
                            batch_bytes,
                            first_key,
                            last_key
                        );
                    }
                    current_bytes = 0;
                    pending_bytes_sum = pending_bytes_sum.saturating_add(batch_bytes);
                    pending_tasks.push(SourcePayloadTask { batches: batch });
                    if pending_tasks.len() >= total_cpus
                        || pending_bytes_sum >= pending_cap_arc.load(AtomicOrdering::Relaxed)
                    {
                        if runtime_log_wal_enabled() {
                            info!(
                                "s3 source: dispatching {} pending source payload tasks total_bytes={}",
                                pending_tasks.len(),
                                pending_bytes_sum
                            );
                        }
                        let submission =
                            ctx.submit_payload_tasks_accepted(std::mem::take(&mut pending_tasks))?;
                        self.active_threads = submission.metrics.active_cores;
                        self.optimal_chunk_size = submission.metrics.optimal_chunk_size;
                        accepted_submissions.push(submission);
                        if accepted_submissions.len() >= 1024 {
                            ctx.wait_payload_acks(&accepted_submissions)?;
                            accepted_submissions.clear();
                        }
                        pending_bytes_sum = 0;
                    }
                }
            }
        }

        if !current_batch.is_empty() {
            let batch_bytes = current_bytes;
            let batch = std::mem::take(&mut current_batch);
            if runtime_log_wal_enabled() {
                let first_key = batch
                    .first()
                    .map(|item| item.offset_key.partition.as_str())
                    .unwrap_or("");
                let last_key = batch
                    .last()
                    .map(|item| item.offset_key.partition.as_str())
                    .unwrap_or("");
                info!(
                    "s3 source: queueing final ingest task objects={} bytes={} first_key={} last_key={}",
                    batch.len(),
                    batch_bytes,
                    first_key,
                    last_key
                );
            }
            pending_tasks.push(SourcePayloadTask { batches: batch });
        }
        if !pending_tasks.is_empty() {
            if runtime_log_wal_enabled() {
                info!(
                    "s3 source: dispatching final {} pending source payload tasks",
                    pending_tasks.len()
                );
            }
            accepted_submissions.push(ctx.submit_payload_tasks_accepted(pending_tasks)?);
        }

        if !accepted_submissions.is_empty() {
            ctx.wait_payload_acks(&accepted_submissions)?;
        }
        ctx.drain_payload_acks()?;
        info!("S3 stream pipeline complete; source payloads submitted to host ingest");
        Ok(())
    }

    /// Download an S3 object with exponential backoff retry logic
    async fn download_s3_object_bytes_with_backoff(
        s3_client: &Client,
        bucket: &String,
        key: &String,
    ) -> Result<Vec<u8>, std::io::Error> {
        let max_retries = 5;
        let mut backoff_duration = Duration::from_millis(1000);
        let mut last_error = String::new();

        for attempt in 1..=max_retries {
            let get_request = s3_client
                .get_object()
                .bucket(bucket.clone())
                .key(urldecode::decode(key.to_string()));

            match get_request.send().await {
                Ok(result) => {
                    let mut data = result.body;
                    let mut data_vec = Vec::new();
                    let mut read_error = None;
                    while let Some(chunk) = data.next().await {
                        match chunk {
                            Ok(bytes) => data_vec.extend_from_slice(&bytes),
                            Err(err) => {
                                read_error = Some(err.to_string());
                                break;
                            }
                        }
                    }
                    if let Some(err) = read_error {
                        last_error = format!(
                            "failed to read S3 object '{}' from bucket '{}': {}",
                            key, bucket, err
                        );
                    } else {
                        return Ok(data_vec);
                    }
                }
                Err(err) => {
                    last_error = format!(
                        "failed to download S3 object '{}' from bucket '{}': {}",
                        key, bucket, err
                    );
                }
            }

            if attempt == max_retries {
                error!("Max retries reached for object {}", key);
                break;
            }

            warn!(
                "Retrying S3 object {} read attempt {}/{} after error: {}",
                key, attempt, max_retries, last_error
            );
            tokio::time::sleep(backoff_duration).await;
            backoff_duration *= 2;
        }

        Err(std::io::Error::other(last_error))
    }
}

#[async_trait]
impl DataSource for DataSourceS3Plugin {
    async fn sync(&mut self, ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error> {
        DataSourceS3Plugin::sync(self, ctx).await
    }

    fn execution_contract(&self) -> skippr_runtime_sdk::plugins::SourceExecutionContract {
        skippr_runtime_sdk::plugins::SourceExecutionContract::finite()
    }
}
