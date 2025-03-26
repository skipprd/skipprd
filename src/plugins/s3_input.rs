use crate::helpers::configuration::{Config, PluginConfig};

use aws_sdk_s3::Client;

use flate2::read::GzDecoder;

use std::io::{Cursor, Read, Write};

use std::sync::{Arc};

use aws_sdk_s3::operation::get_object::{GetObjectError, GetObjectOutput};

use std::time::Duration;
use std::{fs};
use aws_sdk_s3::types::Object;
use futures::future::join_all;

use serde_derive::Deserialize;
use once_cell::sync::Lazy;

use crate::helpers::offsets::{OffsetKey, OffsetTypes, Offsets};
use crate::ingest_work::{Ingest, IngestBatch, ThroughputMetrics};

use tokio::sync::Semaphore;
use crate::helpers::timed_rwlock::TimedRwLock;
use crate::plugins::DataOutputPlugin;
use crate::helpers::Helpers;

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
        let _s3_client = self.s3_client.clone();
        let s3_bucket = self.config.s3_bucket.clone();
        let delimiter = "/".to_string();
        let inventory_prefix = self.config.s3_prefix.clone();
        let max_list_objects = 1000;
        let mut _total_objects = 0;
        let mut chunk_size_current = 0;
        let mut outputs: Vec<String> = Vec::with_capacity(max_list_objects as usize);
        let mut _objects: Vec<Object> = Vec::with_capacity(max_list_objects as usize);
        let mut _continuation_token: Option<String> = Self::load_continuation_token();
        let mut chunks_processed = 0;
        let total_cpus = num_cpus::get();

        println!(
            "Syncing bucket: {}, prefix: {}",
            s3_bucket.clone(),
            inventory_prefix
        );

        // Use a fixed chunk size
        let chunk_size = self.config.batch_size_bytes.clone().unwrap_or(10_000_000) as usize;
        self.optimal_chunk_size = chunk_size;

        let mut s3_prefix = inventory_prefix.trim_start_matches(&delimiter).to_string();

        if s3_prefix == delimiter || s3_prefix == format!(".{}", delimiter) {
            s3_prefix = "".to_string();
        }

        let mut list_obj_req = self
            .s3_client
            .list_objects_v2()
            .bucket(s3_bucket.clone())
            .prefix(s3_prefix.clone())
            .max_keys(max_list_objects);

        // Set initial continuation token if loaded
        if let Some(token) = &_continuation_token {
            println!("Resuming from saved continuation token");
            list_obj_req = list_obj_req.set_continuation_token(Some(token.clone()));
        }

        let max_empty_objects = 2;
        let mut empty_objects_trys = 0;
        
        println!("Starting sync with {} total CPUs and initial chunk size of {}", 
                total_cpus, 
                Helpers::human_readable_size(chunk_size as u64));

        'outer: loop {
            let mut _i = 0;
            let mut _list_retries = 0;
            let mut _skipped_objects = 0;

            // chunk_size = self.optimal_chunk_size;

            match list_obj_req.clone().send().await {
                Err(err) => {
                    println!("S3 Error: {:?}", err);
                    if _list_retries >= 5 {
                        println!("Max retries reached for S3 ListObjectsV2");
                        break 'outer;
                    }
                    _list_retries += 1;
                    tokio::time::sleep(Duration::from_secs(5 * _list_retries)).await;
                },
                Ok(output) => {
                    _list_retries = 0;

                    _objects = match output.contents {
                        Some(o) => o,
                        None => {
                            if empty_objects_trys >= max_empty_objects {
                                println!("No more objects found in S3, skipping Bucket: {} Prefix: {}", s3_bucket, s3_prefix);
                                break 'outer;
                            }
                            empty_objects_trys += 1;
                            continue;
                        }
                    };

                    if !_objects.is_empty() {
                        _skipped_objects = 0;
                        _total_objects += _objects.len();

                        // Process objects
                        for object in _objects {
                            let object_key = object.key().unwrap();
                            let _timestamp = object.last_modified().unwrap().secs();

                            let offset_key = OffsetKey {
                                namespace: s3_bucket.clone(),
                                partition: object_key.to_string(),
                            };

                            let has_offsets = offsets_clone.validate(&offset_key, OffsetTypes::Closed, 1);

                            if Some(true) != has_offsets {
                                outputs.push(object_key.to_string());
                                chunk_size_current += object.size().unwrap_or_default();
                                _i += 1;

                                // If we have enough data for a chunk, process it
                                if chunk_size_current >= self.optimal_chunk_size as i64 {

                                    chunks_processed += 1;
                                    
                                    // Process the current batch
                                    let _throughput_metrics = self.download_and_ingest(
                                        &s3_bucket,
                                        &outputs,
                                        &offsets_clone,
                                        shared_output.clone()
                                    ).await;
                                    
                                    outputs.clear();
                                    _i = 0;
                                    chunk_size_current = 0;
                                }
                            } else {
                                _skipped_objects += 1;
                            }
                        }
                    }

                    if let Some(token) = &output.next_continuation_token {
                        _continuation_token = Some(token.to_string().clone());
                        list_obj_req = list_obj_req.set_continuation_token(_continuation_token.clone());
                        // Save the continuation token after each successful request
                        Self::save_continuation_token(token);
                    } else {
                        if !outputs.is_empty() {
                            chunks_processed += 1;
                            
                            // Process remaining items
                            let _throughput_metrics = self.download_and_ingest(
                                &s3_bucket,
                                &outputs,
                                &offsets_clone,
                                shared_output.clone()
                            ).await;
                        }
                        break 'outer;
                    }
                }
            }
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
        keys: &Vec<String>,
        offsets: &Arc<Offsets>,
        shared_output: Arc<Box<dyn DataOutputPlugin + Send + Sync>>,
    ) -> ThroughputMetrics {
        if keys.is_empty() {
            return ThroughputMetrics {
                bytes_per_second: 0,
                active_cores: self.active_threads,
                queue_length: 0,
                optimal_chunk_size: self.optimal_chunk_size,
            };
        }
        
        let s3_client = self.s3_client.clone();

        // Use a semaphore to limit concurrent downloads
        let semaphore = Arc::new(Semaphore::new(2048));

        // Pre-allocate futures vector with known size
        let futures: Vec<_> = keys
            .clone()
            .into_iter()
            .map(|object_key| {
                let semaphore = Arc::clone(&semaphore);
                let s3_client = s3_client.clone();
                let bucket_name = s3_bucket.to_owned();

                tokio::spawn(async move {
                    let _permit = semaphore.acquire().await.unwrap();

                    match Self::download_s3_object_with_backoff(
                        &s3_client,
                        &bucket_name,
                        &object_key,
                    )
                        .await
                    {
                        Ok(response) => Ok(Download {
                            key: object_key,
                            response,
                        }),
                        Err(_) => Err("Could not get object"),
                    }
                })
            })
            .collect();

        let datas: Arc<TimedRwLock<Vec<IngestBatch>>> = Arc::new(TimedRwLock::new("datas".to_string(), Vec::with_capacity(futures.len())));

        let future_result = join_all(futures).await;

        let bucket_name = s3_bucket.clone();
        let datas_clone = datas.clone();

        // Process the downloaded data in larger batches to reduce context switching
        // Group files by extension to process similar files together
        let mut gz_files = Vec::new();
        let mut regular_files = Vec::new();

        for future in future_result {
            match future.unwrap() {
                Ok(download) => {
                    let mut data = download.response.body;

                    // convert the ByteStream into a Vec<u8>
                    let mut data_vec = Vec::new();
                    while let Some(chunk) = data.next().await {
                        data_vec.extend_from_slice(&chunk.unwrap());
                    }

                    if download.key.contains(".gz") {
                        gz_files.push((download.key, data_vec));
                    } else {
                        regular_files.push((download.key, data_vec));
                    }
                }
                Err(err) => {
                    println!("{:?}", err);
                }
            };
        }

        // Process gzip files in a single blocking task
        if !gz_files.is_empty() {
            let datas_clone = datas_clone.clone();
            let bucket_name_clone = bucket_name.clone();
            
            tokio::task::spawn_blocking(move || {
                for (key, data_vec) in gz_files {
                    // Something that implements `std::io::Read`
                    let c = Cursor::new(data_vec);

                    // To inflate on the fly, "pipe" the data through the decoder
                    let mut stream = GzDecoder::new(c);

                    let mut decompressed_data = String::new();
                    stream.read_to_string(&mut decompressed_data).unwrap();

                    datas_clone.write().push(IngestBatch {
                        offset_key: OffsetKey {
                            namespace: bucket_name_clone.to_string(),
                            partition: key,
                        },
                        data: decompressed_data,
                    });
                }
            }).await.unwrap();
        }

        // Process regular files in a single blocking task
        if !regular_files.is_empty() {
            let datas_clone = datas_clone.clone();
            let bucket_name_clone = bucket_name.clone();
            
            tokio::task::spawn_blocking(move || {
                for (key, data_vec) in regular_files {
                    let str_data = String::from_utf8(data_vec).unwrap();

                    datas_clone.write().push(IngestBatch {
                        offset_key: OffsetKey {
                            namespace: bucket_name_clone.to_string(),
                            partition: key,
                        },
                        data: str_data,
                    });
                }
            }).await.unwrap();
        }

        let batch = datas.read().clone();
        let shared_output_clone = shared_output.clone();
        
        // Process the batch and get throughput metrics
        let metrics = self.ingest.ingest_file(&Arc::new(batch), &offsets, shared_output_clone);
        
        // Update our active threads count
        self.active_threads = metrics.active_cores;
        self.optimal_chunk_size = metrics.optimal_chunk_size;

        // Return the metrics for the caller
        ThroughputMetrics {
            bytes_per_second: metrics.bytes_per_second,
            active_cores: metrics.active_cores,
            queue_length: metrics.queue_length,
            optimal_chunk_size: metrics.optimal_chunk_size,
        }
    }
}

/// Helper struct to track a downloaded S3 object
struct Download {
    key: String,
    response: GetObjectOutput,
}

