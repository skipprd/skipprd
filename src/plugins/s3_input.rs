use crate::helpers::configuration::{Config, PluginConfig};

use aws_sdk_s3::Client;

use flate2::read::GzDecoder;

use std::io::{Cursor, Read, Write};

use std::sync::{Arc};

use aws_sdk_s3::operation::get_object::{GetObjectError, GetObjectOutput};



use std::time::Duration;
use std::{fs, io};
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};


use futures::future::join_all;
use futures::{StreamExt};
use nix::unistd::sleep;

use serde_derive::Deserialize;
use once_cell::sync::Lazy;

use crate::helpers::offsets::{OffsetKey, OffsetTypes, Offsets};
use crate::ingest_work::{Ingest, IngestBatch};

use tokio::sync::Semaphore;
use crate::helpers::timed_rwlock::TimedRwLock;
use crate::plugins::DataOutputPlugin;

// in data_dir
const CONTINUATION_TOKEN_FILE: Lazy<String> = Lazy::new(|| {
    format!("{}/s3_continuation_token", Config::get_data_dir())
});

#[derive(Deserialize, Debug, Clone)]
pub struct DataSourceS3PluginConfig {
    pub format: Option<String>,
    pub batch_size_seconds: Option<i64>,
    pub batch_size_bytes: Option<i64>,

    pub s3_bucket: String,
    pub s3_prefix: String,
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
    continuation_tokens: VecDeque<String>,
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
            }
        };

        let continuation_tokens = Self::read_continuation_token().unwrap_or_else(|_| Err(io::Error::new(io::ErrorKind::NotFound, "Failed to get S3 Input continuation token locally")).unwrap());

        DataSourceS3Plugin {
            s3_client,
            ingest: Ingest::new(),
            config,
            temp_dir: temp_dir.to_string(),
            continuation_tokens
        }
    }

    pub async fn sync(
        &mut self,
        offsets: Arc<Offsets>,
        shared_output: Arc<TimedRwLock<Box<dyn DataOutputPlugin + Send + Sync>>>,
    ) {

        // let offsets = Arc::new(Offsets::init().unwrap());

        let offsets_clone = offsets.clone();

        // let offset_key = OffsetKey { namespace: "foofg".to_string(), partition: "bar".to_string() };
        //
        // let res = offsets_clone.validate(&offset_key, OffsetTypes::Closed, 0);
        // panic!("{:?}", res);

        let _s3_client = self.s3_client.clone();
        // let s3_client = s3_client.clone();

        // let s3_client = Arc::new(s3_client);

        // let mut outputs: HashMap<String, Vec<String>> = HashMap::new();
        let mut outputs: Vec<String> = Vec::new();

        let s3_bucket = self.config.s3_bucket.clone();
        let inventory_prefix = self.config.s3_prefix.clone();

        println!(
            "Syncing from bucket: {} and prefix {}",
            s3_bucket.clone(),
            inventory_prefix
        );

        let mut skipped_objects = 0;

        let chunk_size = self.config.batch_size_bytes.clone().unwrap_or(10000000);

        let mut i = 0;
        let mut chunk_size_current = 0;

        let mut s3_prefix = inventory_prefix.trim_start_matches('/').to_string();

        if s3_prefix == "/".to_string() || s3_prefix == "./".to_string() {
            s3_prefix = "".to_string();
        }

        let max_list_objects = 1000;

        let mut continuation_token: Option<String> = self.continuation_tokens.front().cloned();

        let mut list_obj_req = self
            .s3_client
            .list_objects_v2()
            .bucket(s3_bucket.clone())
            .prefix(s3_prefix.clone())
            .max_keys(max_list_objects);


        if let Some(token) = &continuation_token {
            // println!("Continuing from token: {:?}", token);
            list_obj_req = list_obj_req.set_continuation_token(Some(token.clone()));
        }

        // important to check few times, else slowly arriving drip of objects will result in us never proceeding to the next pipeline
        let max_empty_objects = 2;
        let mut empty_objects = 0;

        loop {

            i += 1;

            match list_obj_req.clone().send().await {
                Err(err) => {
                    println!("S3 Error: {:?}", err);
                },
                Ok(output) => {

                    let objects = match output.contents() {
                        Some(objects) => {

                            // println!("Current continuation token: {:?}", continuation_token.clone());

                            // If current continuation token has been previously saved, it means we've skipped
                            // this page before. So we can delete the offsets for the objects in the Sled offsets DB
                            if continuation_token.is_some() &&
                                self.continuation_tokens.contains(&continuation_token.clone().unwrap()) {

                                println!("Rolling up offsets database, will vacuum to recover for disk space");

                                let mut count = 0;

                                for object in objects {
                                    let offset_key = OffsetKey {
                                        namespace: s3_bucket.clone(),
                                        partition: object.key().unwrap().to_string(),
                                    };
                                    match offsets_clone.remove(&offset_key) {
                                        Ok(old_val) => {
                                            if old_val.is_some() {
                                                count += 1;
                                            }
                                        },
                                        Err(err) => {
                                            println!("Failed to rollup offset database, Error: {:?}", err);
                                            return;
                                        }
                                    }
                                }

                                sleep(1);
                                // Not, sled doesn't delete the keys, just nulls the values.
                                // So we vacuum the db on startup
                                println!("Rolled up {} offsets", count);
                                offsets_clone.flush().unwrap();

                                // remove the token. @todo - should be able to pop front?
                                self.continuation_tokens.iter().position(|x| x == &continuation_token.clone().unwrap()).map(|i| {
                                    self.continuation_tokens.remove(i);
                                });

                                continue;
                            }

                            objects
                        },
                        None => {
                            if empty_objects >= max_empty_objects {
                                println!("No more objects found in S3, skipping Bucket: {} Prefix: {}", s3_bucket, s3_prefix);
                                break;
                            }
                            empty_objects += 1;
                            continue;
                        }
                    };

                    if !objects.is_empty() {

                        skipped_objects = 0;

                        // println!("Processing {} objects", objects.len());

                        for object in objects {
                            // if object.is_empty() || object == null {
                            //     SkipprLogger::debug("object has no key");
                            //     continue;
                            // }

                            let object_key = object.key().unwrap();
                            let _timestamp = object.last_modified().unwrap().secs();

                            // println!("Object Key {}", object_key);

                            // let mut j = 0;
                            // let mut c = 0;
                            //
                            // let inventorys = vec![];

                            // let records_total = rdr.records().count();

                            let offset_key = OffsetKey {
                                namespace: s3_bucket.clone(),
                                partition: object_key.to_string(),
                            };

                            // Check offset is not already processed
                            let has_offsets= offsets_clone.validate(&offset_key, OffsetTypes::Closed, 1);

                            // println!("has_offsets: {:?}", has_offsets);

                            if Some(true) != has_offsets {
                                // println!("getting key: {}", object_key);

                                outputs.push(object_key.to_string());

                                chunk_size_current += object.size();

                                i += 1;

                                // println!("State {} = {} of {}", i, chunk_size_current, chunk_size);

                                if chunk_size_current >= chunk_size {
                                    // println!("Proccessing {} Objects, totalling {} bytes (batch size config {} bytes)", i, chunk_size_current, chunk_size);

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

                        if skipped_objects > 0 {
                            if skipped_objects >= max_list_objects {
                                // println!("Skipped {} objects... already processed.", skipped_objects);

                                // Save continuation token if it's not the last page
                                // and the token hasn't been saved before
                                if let Some(new_token) = &output.next_continuation_token {

                                    if continuation_token.is_some() &&
                                        !self.continuation_tokens.contains(&continuation_token.clone().unwrap()) {

                                        // println!("Saving continuation token: {}", &continuation_token.clone().unwrap());

                                        self.continuation_tokens.push_back(continuation_token.unwrap().clone());
                                        Self::save_continuation_token(&Some(self.continuation_tokens.clone())).unwrap();
                                    }

                                }

                                skipped_objects = 0;

                                // continuation tokens aren't consistent hashes, however a token will
                                // imdempotently return the same results if the list of objects hasn't changed
                                // If the list request returns a token, indicating there is more data, first use
                                // our next saved token for the next page so we can rollup the offsets database
                                // If we don't have a saved token, use the token returned by the list request
                                if self.continuation_tokens.len() > 0 {
                                    continuation_token = self.continuation_tokens.front().cloned();
                                    println!("Continuation token: {:?}", &continuation_token.clone().unwrap());
                                    list_obj_req = list_obj_req.set_continuation_token(Some(continuation_token.clone().unwrap()));
                                    continue;
                                }

                            }
                        }
                    }

                    if let Some(token) = &output.next_continuation_token {

                        continuation_token = Some(token.to_string().clone());

                        println!("Continuation token: {:?}", continuation_token.clone());

                        list_obj_req = list_obj_req.set_continuation_token(Some(token.to_string().clone()));


                    } else {
                        println!("Reached end of S3 pagination");

                        if !outputs.is_empty() {
                            self.download_and_ingest(
                                &s3_bucket,
                                &outputs,
                                &offsets_clone,
                                shared_output.clone()
                            )
                                .await;
                        }

                        break;
                    }

                }
            }
        }
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

    fn save_continuation_token(token: &Option<VecDeque<String>>) -> io::Result<()> {
        match token {
            Some(t) => {
                let mut file = File::create(CONTINUATION_TOKEN_FILE.to_string())?;
                file.write_all(serde_json::to_string(t).unwrap().as_bytes())?;
                file.flush()?;
                Ok(())
            },
            None => Err(io::Error::new(io::ErrorKind::NotFound, "No token to save")),
        }
    }

    fn read_continuation_token() -> io::Result<VecDeque<String>> {
        match OpenOptions::new().read(true).write(true).create(true).open(CONTINUATION_TOKEN_FILE.to_string()) {
            Ok(mut file) => {
                let mut buf = String::new();
                file.read_to_string(&mut buf)?;
                println!("Found previous S3 List continuation tokens: {}", buf);
                let token: VecDeque<String> = serde_json::from_str(&buf).unwrap_or(VecDeque::new());
                Ok(token)
            },
            Err(_) => Err(io::Error::new(io::ErrorKind::NotFound, "No token found")),
        }
    }
}

struct Download {
    key: String,
    response: GetObjectOutput,
}
