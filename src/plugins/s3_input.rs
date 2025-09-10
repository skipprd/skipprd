use crate::helpers::configuration::{Config, PluginConfig};

use aws_sdk_s3::Client;

use flate2::read::GzDecoder;

use std::io::{Cursor, Read, Write};

use std::sync::{Arc};

use aws_sdk_s3::operation::get_object::{GetObjectError, GetObjectOutput};

use std::time::Duration;
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
// use futures::StreamExt;

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
        let offsets_clone = offsets.clone();
        let s3_bucket = self.config.s3_bucket.clone();
        let delimiter = "/".to_string();
        let inventory_prefix = self.config.s3_prefix.clone();
        let max_list_objects = 1000;
        let total_cpus = num_cpus::get();

        println!(
            "Syncing bucket: {}, prefix: {}",
            s3_bucket.clone(),
            inventory_prefix
        );

        // Initialize chunk size with configured batch size
        let chunk_size = self.config.batch_size_bytes.unwrap_or(10_000_000) as usize;
        self.optimal_chunk_size = chunk_size;

        let mut s3_prefix = inventory_prefix.trim_start_matches(&delimiter).to_string();
        if s3_prefix == delimiter || s3_prefix == format!(".{}", delimiter) {
            s3_prefix = "".to_string();
        }

        // Stage channels
        let (keys_tx, mut keys_rx) = mpsc::channel::<(String, i64)>(std::cmp::max(32, total_cpus * 4));
        let (files_tx, mut files_rx) = mpsc::channel::<DownloadedItem>(std::cmp::max(16, total_cpus * 2));

        // Memory byte-budget semaphore (1 permit = 1 MiB)
        let mem_budget_mb: u32 = Config::getenv("S3_DOWNLOAD_MEMORY_MB", "4096")
            .parse::<u32>()
            .unwrap_or(4096);
        let mem_sem = Arc::new(Semaphore::new(mem_budget_mb as usize));

        println!(
            "Starting bounded pipeline (cpus={}, dl_mem={} MiB, init_chunk={})",
            total_cpus,
            mem_budget_mb,
            Helpers::human_readable_size(chunk_size as u64)
        );

        // Lister stage
        let s3_client_list = self.s3_client.clone();
        let s3_bucket_list = s3_bucket.clone();
        let mut list_obj_req = s3_client_list
            .list_objects_v2()
            .bucket(s3_bucket_list.clone())
            .prefix(s3_prefix.clone())
            .max_keys(max_list_objects);

        // Set initial continuation token if loaded
        if let Some(token) = &Self::load_continuation_token() {
            println!("Resuming from saved continuation token");
            list_obj_req = list_obj_req.set_continuation_token(Some(token.clone()));
        }

        let offsets_for_lister = offsets_clone.clone();
        let lister_handle = {
            let keys_tx = keys_tx.clone();
            tokio::spawn(async move {
                let mut _list_retries = 0;
                let max_empty_objects = 2;
                let mut empty_objects_trys = 0;
                let mut _continuation_token: Option<String> = None;

                loop {
                    match list_obj_req.clone().send().await {
                        Err(err) => {
                            println!("S3 Error: {:?}", err);
                            if _list_retries >= 5 {
                                println!("Max retries reached for S3 ListObjectsV2");
                                break;
                            }
                            _list_retries += 1;
                            tokio::time::sleep(Duration::from_secs(5 * _list_retries)).await;
                        }
                        Ok(output) => {
                            _list_retries = 0;
                            let objects = match output.contents {
                                Some(o) => o,
                                None => {
                                    if empty_objects_trys >= max_empty_objects {
                                        println!("No more objects found in S3, skipping Bucket: {} Prefix: {}", s3_bucket_list, s3_prefix);
                                        break;
                                    }
                                    empty_objects_trys += 1;
                                    continue;
                                }
                            };

                            for object in objects {
                                let object_key = match object.key() { Some(k) => k.to_string(), None => continue };
                                let offset_key = OffsetKey {
                                    namespace: s3_bucket_list.clone(),
                                    partition: object_key.clone(),
                                };
                                if Some(true) == offsets_for_lister.validate(&offset_key, OffsetTypes::Closed, 1) {
                                    continue;
                                }

                                let sz = object.size().unwrap_or_default();
                                if let Err(_e) = keys_tx.send((object_key, sz)).await {
                                    // receiver dropped
                                    break;
                                }
                            }

                            if let Some(token) = &output.next_continuation_token {
                                _continuation_token = Some(token.to_string().clone());
                                list_obj_req = list_obj_req.set_continuation_token(_continuation_token.clone());
                                Self::save_continuation_token(token);
                            } else {
                                break;
                            }
                        }
                    }
                }
                // drop sender to signal completion
                drop(keys_tx);
            })
        };

        // Downloader supervisor: single receiver, per-key tasks limited by dl_sem
        let downloader_concurrency = std::cmp::min(256, total_cpus * 8);
        let dl_sem = Arc::new(Semaphore::new(downloader_concurrency as usize));
        let mut join_set: JoinSet<()> = JoinSet::new();
        let s3_client_dl = self.s3_client.clone();
        let s3_bucket_dl = s3_bucket.clone();
        let files_tx_dl = files_tx.clone();
        let mem_sem_dl = mem_sem.clone();
        let downloader_supervisor = tokio::spawn(async move {
            while let Some((key, size_bytes)) = keys_rx.recv().await {
                // Acquire download concurrency permit
                let dl_permit = match dl_sem.clone().acquire_owned().await {
                    Ok(p) => p,
                    Err(_) => break,
                };

                let s3_client = s3_client_dl.clone();
                let bucket = s3_bucket_dl.clone();
                let files_tx = files_tx_dl.clone();
                let mem_sem = mem_sem_dl.clone();
                let mem_budget = mem_budget_mb;

                join_set.spawn(async move {
                    // Compute memory permits in MiB (at least 1)
                    let mut permits: u32 = ((std::cmp::max(1i64, size_bytes) as u64 + 1_048_575) / 1_048_576) as u32;
                    if permits == 0 { permits = 1; }
                    if permits > mem_budget { permits = mem_budget; }

                    if let Ok(permit) = mem_sem.acquire_many_owned(permits).await {
                        match DataSourceS3Plugin::download_s3_object_with_backoff(&s3_client, &bucket, &key).await {
                            Ok(response) => {
                                let mut data = response.body;
                                let mut data_vec = Vec::new();
                                while let Some(chunk) = data.next().await {
                                    if let Ok(bytes) = chunk { data_vec.extend_from_slice(&bytes); }
                                }
                                let is_gz = key.contains(".gz");
                                let item = DownloadedItem { key, data: data_vec, is_gz, permit };
                                let _ = files_tx.send(item).await;
                            }
                            Err(_e) => {
                                // download failed; permit dropped here
                            }
                        }
                    }

                    // drop dl_permit at end of task
                    drop(dl_permit);
                });
            }

            // Drain remaining tasks
            while let Some(_res) = join_set.join_next().await {}
        });

        // Aggregator stage runs inline so we can update self safely
        let mut current_batch: IngestTask = IngestTask::new(Vec::new(), offsets.clone(), shared_output.clone());
        let mut current_bytes: usize = 0;
        loop {
            tokio::select! {
                maybe_item = files_rx.recv() => {
                    match maybe_item {
                        Some(item) => {
                            let DownloadedItem { key, data, is_gz, permit } = item;

                            let str_data_res: Result<String, String> = if is_gz {
                                match tokio::task::spawn_blocking(move || {
                                    let c = Cursor::new(data);
                                    let mut stream = GzDecoder::new(c);
                                    let mut decompressed = String::new();
                                    match stream.read_to_string(&mut decompressed) {
                                        Ok(_) => Ok(decompressed),
                                        Err(e) => Err(format!("Gzip decode failed: {}", e))
                                    }
                                }).await {
                                    Ok(r) => r,
                                    Err(e) => Err(format!("Join error: {:?}", e))
                                }
                            } else {
                                match String::from_utf8(data) {
                                    Ok(s) => Ok(s),
                                    Err(e) => Err(format!("UTF-8 decode failed: {}", e))
                                }
                            };

                            drop(permit);

                            let str_data = match str_data_res {
                                Ok(s) => s,
                                Err(err) => { println!("Skipping file {} due to decode error: {}", key, err); continue; }
                            };

                            let bytes = str_data.len();
                            current_bytes += bytes;

                            let mut new_datas = Vec::new();
                            new_datas.extend(current_batch.datas.iter().cloned());
                            new_datas.push(IngestBatch {
                                offset_key: OffsetKey { namespace: s3_bucket.clone(), partition: key },
                                data: str_data,
                                bytes,
                            });
                            current_batch.datas = Arc::new(new_datas);

                            if current_bytes >= self.optimal_chunk_size {
                                let mut tasks = IngestTasks::new();
                                tasks.add(current_batch);
                                let tasks_arc = Arc::new(tasks);
                                let metrics = self.ingest.ingest_file(&tasks_arc, &offsets, shared_output.clone());
                                self.active_threads = metrics.active_cores;
                                self.optimal_chunk_size = metrics.optimal_chunk_size;
                                current_batch = IngestTask::new(Vec::new(), offsets.clone(), shared_output.clone());
                                current_bytes = 0;
                            }
                        },
                        None => break,
                    }
                }
            }
        }

        // Wait for lister and downloader supervisor to finish
        let _ = lister_handle.await;
        let _ = downloader_supervisor.await;

        // Flush any remaining
        if !current_batch.datas.is_empty() {
            let mut tasks = IngestTasks::new();
            tasks.add(current_batch);
            let tasks_arc = Arc::new(tasks);
            let _ = self.ingest.ingest_file(&tasks_arc, &offsets, shared_output.clone());
        }
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

