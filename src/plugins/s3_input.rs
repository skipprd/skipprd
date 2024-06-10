use crate::helpers::configuration::{Config, PluginConfig};

use aws_sdk_s3::Client;

use flate2::read::GzDecoder;

use std::io::{Cursor, Read, Write};

use std::sync::{Arc};

use aws_sdk_s3::operation::get_object::{GetObjectError, GetObjectOutput};



use std::time::Duration;
use std::{fs, io};
use std::collections::{HashSet};
use std::fs::{ OpenOptions};

use futures::future::join_all;
use futures::{StreamExt};

use serde_derive::Deserialize;
use once_cell::sync::Lazy;

use crate::helpers::offsets::{OffsetKey, OffsetTypes, Offsets};
use crate::ingest_work::{Ingest, IngestBatch};

use tokio::sync::Semaphore;
use crate::helpers::timed_rwlock::TimedRwLock;
use crate::plugins::DataOutputPlugin;

// in data_dir
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
    pub s3_prefix_ordered_depth: Option<usize>,
    pub s3_delimiter: Option<String>,
}

impl From<PluginConfig> for DataSourceS3PluginConfig {
    fn from(plugin_config: PluginConfig) -> Self {
        match plugin_config {
            PluginConfig::s3(s3_config) => s3_config,
            _ => panic!("Invalid plugin type"),
        }
    }
}

pub struct DataSourceS3Plugin {
    // config: HashMap<String, String>,
    // buffer: Sender<String>,
    s3_client: Client,
    // s3_client_rusoto: S3Client,
    ingest: Ingest,
    config: DataSourceS3PluginConfig,
    temp_dir: String,
    prefixes: Vec<(String, usize)>,
}

impl DataSourceS3Plugin {
    pub async fn new() -> DataSourceS3Plugin {
        let s3_config = aws_config::from_env().load().await;

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
        }
    }

    pub async fn sync(
        &mut self,
        offsets: Arc<Offsets>,
        shared_output: Arc<TimedRwLock<Box<dyn DataOutputPlugin + Send + Sync>>>,
    ) {


        let offsets_clone = offsets.clone();

        let _s3_client = self.s3_client.clone();

        let s3_bucket = self.config.s3_bucket.clone();
        
        let max_depth = self.config.s3_prefix_ordered_depth.clone().unwrap_or(0);
        let delimiter = self.config.s3_delimiter.clone().unwrap_or("/".to_string());
        
        let mut seen: HashSet<String> = HashSet::new();
        
        self.prefixes.push((self.config.s3_prefix.clone(), 0));

        let mut total_objects = 0;
        let mut chunk_size_current = 0;

        while !self.prefixes.is_empty() {

            let (inventory_prefix, prefix_depth) = self.prefixes.pop().unwrap();

            let mut continuation_token = Self::read_continuation_token(offsets.clone(), s3_bucket.clone(), inventory_prefix.clone()).unwrap_or_else(|_| Err(io::Error::new(io::ErrorKind::NotFound, "Failed to get S3 Input continuation token from Offset DB")).unwrap());

            println!(
                "Syncing bucket: {}, prefix: {}",
                s3_bucket.clone(),
                inventory_prefix
            );

            let chunk_size = self.config.batch_size_bytes.clone().unwrap_or(10000000);

            let mut s3_prefix = inventory_prefix.trim_start_matches('/').to_string();

            if s3_prefix == delimiter || s3_prefix == format!(".{}", delimiter) {
                s3_prefix = "".to_string();
            }
            
            let max_list_objects = 1000;

            let mut list_obj_req = self
                .s3_client
                .list_objects_v2()
                .bucket(s3_bucket.clone())
                .prefix(s3_prefix.clone())
                .max_keys(max_list_objects);

            if prefix_depth < max_depth {
                list_obj_req = list_obj_req.delimiter(&delimiter);
            }
            
            if let Some(token) = &continuation_token {
                // println!("Continuing from token: {:?}", token);
                list_obj_req = list_obj_req.set_continuation_token(Some(token.clone()));
            }

            // important to check few times, else slowly arriving drip of objects will result in us never proceeding to the next pipeline
            let max_empty_objects = 2;
            let mut empty_objects_trys = 0;

            let mut outputs: Vec<String> = Vec::new();


            loop {

                let mut i = 0;


                let mut skipped_objects = 0;
                
                match list_obj_req.clone().send().await {
                    Err(err) => {
                        println!("S3 Error: {:?}", err);
                    },
                    Ok(output) => {

                        let common_prefixes = output.common_prefixes();

                        for prefix in common_prefixes.iter().filter_map(|p| p.prefix()) {
                            let depth = prefix.matches(&delimiter).count();
                            if depth <= max_depth && seen.insert(prefix.to_string()) {
                                // println!("Adding prefix: {} at depth: {}", prefix, depth);
                                self.prefixes.push((prefix.to_string(), depth));
                            }
                        }
                        if output.contents.is_none() {
                            if empty_objects_trys >= max_empty_objects {
                                // println!("No more objects found in S3, skipping Bucket: {} Prefix: {}", s3_bucket, s3_prefix);
                                break;
                            }
                            empty_objects_trys += 1;
                            continue;
                        };
                        
                        let objects = output.contents();

                        // println!("Sub-Syncing bucket: {}, prefix: {}", s3_bucket, s3_prefix);
                        // println!("Found {} objects in S3", objects.len());
                        // println!("Continuation token: {:?}", output.next_continuation_token);

                        if !objects.is_empty() {
                            skipped_objects = 0;

                            total_objects += objects.len();

                            for object in objects {

                                let object_key = object.key().unwrap();
                                let _timestamp = object.last_modified().unwrap().secs();

                                // println!("Processing object: {}", object_key);

                                let offset_key = OffsetKey {
                                    namespace: s3_bucket.clone(),
                                    partition: object_key.to_string(),
                                };
                                // Check offset is not already processed
                                let has_offsets = offsets_clone.validate(&offset_key, OffsetTypes::Closed, 1);

                                if Some(true) != has_offsets {

                                    outputs.push(object_key.to_string());

                                    chunk_size_current += object.size().unwrap_or_default();

                                    i += 1;

                                    if chunk_size_current >= chunk_size {

                                        self.download_and_ingest(
                                            &s3_bucket,
                                            &outputs,
                                            &offsets_clone,
                                            shared_output.clone()
                                        )
                                            .await;

                                        outputs = Vec::new();
                                        i = 0;
                                        chunk_size_current = 0;
                                    }
                                } else {
                                    skipped_objects += 1;
                                }
                            }
                            
                            // println!("Skipped objects: {}", skipped_objects);
                            
                            if skipped_objects >= max_list_objects {

                                
                                // println!("Next continuation token: {:?}", output.next_continuation_token);
                                
                                // If we've skipped all objects in the list request then we can save the continuation token
                                if continuation_token.is_some()
                                    && output.next_continuation_token.is_some() // If there is a next token, we can save the current one. 
                                {

                                    Self::save_continuation_token(
                                        &output.next_continuation_token,
                                        offsets_clone.clone(),
                                        s3_bucket.clone(),
                                        inventory_prefix.clone()
                                    ).unwrap();

                                    // // rollup the offsets database
                                    // if log_rollup {
                                    //     println!("Rolling up offsets database to recover disk space");
                                    //     log_rollup = false;
                                    // }
                                    //
                                    // for object in objects {
                                    //     let offset_key = OffsetKey {
                                    //         namespace: s3_bucket.clone(),
                                    //         partition: object.key().unwrap().to_string(),
                                    //     };
                                    //     match offsets_clone.remove(&offset_key) {
                                    //         Ok(old_val) => {},
                                    //         Err(err) => {
                                    //             println!("Failed to rollup offset database, Error: {:?}", err);
                                    //             break;
                                    //         }
                                    //     }
                                    // }
                                    //
                                    // // Not, sled doesn't delete the keys, just nulls the values.
                                    // // So we  need to vacuum the db on startup to give as chance for sled GC to run
                                    // offsets_clone.flush().unwrap();
                                }

                            }
                        }

                        if let Some(token) = &output.next_continuation_token {
                            continuation_token = Some(token.to_string().clone());

                            list_obj_req = list_obj_req.set_continuation_token(continuation_token.clone());
                        } else {

                            if !outputs.is_empty() {
                                self.download_and_ingest(
                                    &s3_bucket,
                                    &outputs,
                                    &offsets_clone,
                                    shared_output.clone()
                                )
                                    .await;
                            }
                            
                            break
                        }
                    }
                }
            }
        }

        println!("Reached end of S3 pagination");

        // println!("Total objects: {}", total_objects);

    }


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
                    if retries > 0 {
                        // println!("Successful retry of object {}", key);
                    }
                    return Ok(result);
                }
                Err(err) => {

                    retries += 1;

                    backoff_duration *= 2;

                    // println!(
                    //     "Failed to get object {}, retry {} of {} in {} seconds: {}",
                    //     key,
                    //     retries,
                    //     max_retries,
                    //     backoff_duration.as_secs(),
                    //     err.to_string()
                    // );

                    // let wait_time = backoff_duration.as_secs_f64() * 2.0_f64.powi(retries);
                    tokio::time::sleep(Duration::from_secs_f64(backoff_duration.as_secs_f64())).await;

                    if retries >= max_retries {
                        println!("Max retries reached for object {}", key);
                        return Err(err.into_service_error());
                    }
                }
            }
        }
    }

    async fn download_and_ingest(
        &self,
        bucket_name: &String,
        object_keys: &Vec<String>,
        offsets_clone: &Arc<Offsets>,
        shared_output: Arc<TimedRwLock<Box<dyn DataOutputPlugin + Send + Sync>>>,
    ) {
        let s3_client = self.s3_client.clone();

        let semaphore = Arc::new(Semaphore::new(2048));

        let futures: Vec<_> = object_keys
            .clone()
            .into_iter()
            .map(|object_key| {
                let semaphore = Arc::clone(&semaphore);
                let s3_client = s3_client.clone();
                let bucket_name = bucket_name.to_owned();

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
                    // We drop the permit here, allowing another future to acquire it
                })
            })
            .collect();

        let datas: Arc<TimedRwLock<Vec<IngestBatch>>> = Arc::new(TimedRwLock::new("datas".to_string(), Vec::new()));

        let future_result = join_all(futures).await;

        let bucket_name = bucket_name.clone();
        let datas_clone = datas.clone();

        for future in future_result {
            match future.unwrap() {
                Ok(download) => {

                    // println!("Downloading s3 object");
                    let mut data = download.response.body;

                    // convert the ByteStream into a Vec<u8>
                    let mut data_vec = Vec::new();
                    while let Some(chunk) = data.next().await {
                        data_vec.extend_from_slice(&chunk.unwrap());
                    }

                    let datas_clone = datas_clone.clone();
                    let bucket_name_clone = bucket_name.clone();

                    // Spawning a blocking task to handle CPU-bound decompression
                    tokio::task::spawn_blocking(move || {

                        if download.key.contains(".gz") {
                            // Something that implements `std::io::Read`
                            let c = Cursor::new(data_vec);

                            // To inflate on the fly, "pipe" the data through the decoder, i.e. wrap the reader
                            let mut stream = GzDecoder::new(c);

                            let mut decompressed_data = String::new();
                            stream.read_to_string(&mut decompressed_data).unwrap();

                            datas_clone.write().push(IngestBatch {
                                offset_key: OffsetKey {
                                    namespace: bucket_name_clone.to_string(),
                                    partition: download.key,
                                },
                                data: decompressed_data,
                            });
                        } else {
                            let str_data = String::from_utf8(data_vec).unwrap();

                            datas_clone.write().push(IngestBatch {
                                offset_key: OffsetKey {
                                    namespace: bucket_name_clone.to_string(),
                                    partition: download.key,
                                },
                                data: str_data,
                            });
                        }


                    }).await.unwrap();

                }
                Err(err) => {
                    println!("{:?}", err);
                }
            };
        }

        // @todo - pass datas to ingest_file without cloning
        let batch = datas.read().clone();
        let shared_output_clone = shared_output.clone();
        self.ingest.ingest_file(&Arc::new(batch), &offsets_clone, shared_output_clone);
    }

    fn save_continuation_token(
        token: &Option<String>,
        offsets_clone: Arc<Offsets>,
        s3_bucket: String,
        s3_prefix: String,
    ) -> io::Result<()> {
        
        // println!("Saving continuation token: {} for bucket: {}, prefix: {}", token.clone().unwrap(), s3_bucket, s3_prefix);
        
        match token {
            Some(t) => {

                let offset_key = OffsetKey {
                    namespace: s3_bucket.clone(),
                    partition: s3_prefix.clone(),
                };

                let key = offsets_clone.build_key(&offset_key);
                let key_bytes: &[u8] = &key.as_bytes();

                match offsets_clone.tree.insert(key_bytes, sled::IVec::from(&t.as_bytes()[..])) {
                    Ok(_) => {
                        // println!("Saved continuation token: {} for bucket: {}, prefix: {}", t, s3_bucket, s3_prefix);
                        Ok(())
                    },
                    Err(err) => {
                        Err(io::Error::new(io::ErrorKind::Other, "Failed to save continuation token"))
                    }
                }

            },
            None => Err(io::Error::new(io::ErrorKind::NotFound, "No token to save")),
        }
    }

    fn read_continuation_token(
        offsets_clone: Arc<Offsets>,
        s3_bucket: String,
        s3_prefix: String,
    ) -> io::Result<Option<String>> {
        
        return Ok(None);

        // migrate from old continuation token file
        match OpenOptions::new().read(true).open(CONTINUATION_TOKEN_FILE.to_string()) {
            Ok(mut file) => {
                let mut buf = String::new();
                file.read_to_string(&mut buf)?;
                println!("Found previous S3 List continuation tokens: {}", buf);
                let token: String = serde_json::from_str(&buf).unwrap_or_default();

                // Remove the file, we use offset DB now
                fs::remove_file(CONTINUATION_TOKEN_FILE.to_string()).unwrap();

                if token.is_empty() {
                    return Ok(None);
                } else {
                    return Ok(Some(token));
                }

            },
            Err(_) => (),
        };

        let offset_key = OffsetKey {
            namespace: s3_bucket,
            partition: s3_prefix,
        };
        let key = offsets_clone.build_key(&offset_key);
        let key_bytes: &[u8] = &key.as_bytes();
        
        let offset = offsets_clone.tree.get(&key_bytes).unwrap();
        
        let continuation_token: String = match offset {
            Some(t) => {
                String::from_utf8(t.to_vec()).unwrap()
            },
            _ => {
                return Ok(None);
            }
        };
        
        // println!("Found continuation token: {}", continuation_token);

        Ok(Some(continuation_token))

    }
}

struct Download {
    key: String,
    response: GetObjectOutput,
}

