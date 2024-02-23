use crate::helpers::configuration::{Config, PluginConfig};

use aws_sdk_s3::Client;

pub use aws_smithy_http::byte_stream::AggregatedBytes;

use flate2::read::GzDecoder;

use std::io::{Cursor, Read};

use std::sync::{Arc};

use aws_sdk_s3::operation::get_object::{GetObjectError, GetObjectOutput};



use std::time::Duration;
use std::{fs};
use aws_sdk_athena::config::timeout::TimeoutConfig;
use aws_sdk_s3::config::retry::RetryConfig;


use futures::future::join_all;
use futures::{StreamExt};

use serde_derive::Deserialize;


use crate::helpers::offsets::{OffsetKey, OffsetTypes, Offsets};
use crate::ingest_work::{Ingest, IngestBatch};

use tokio::sync::Semaphore;
use crate::helpers::timed_rwlock::TimedRwLock;


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

impl Into<PluginConfig> for DataSourceS3PluginConfig {
    fn into(self) -> PluginConfig {
        PluginConfig::s3(self)
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
}

impl DataSourceS3Plugin {
    pub async fn new() -> DataSourceS3Plugin {

        let retry_config = RetryConfig::standard().with_max_attempts(5);

        let sdk_config = aws_config::from_env()
            .timeout_config(
                TimeoutConfig::builder()
                    .operation_timeout(Duration::from_secs(5))
                    .operation_attempt_timeout(Duration::from_secs(5))
                    .connect_timeout(Duration::from_secs(5))
                    .build()
            )
            .retry_config(retry_config)
        .load().await;

        let s3_config = aws_sdk_s3::config::Builder::from(&sdk_config)
            .build();

        let data_dir = Config::get_data_dir();
        let temp_dir = &format!("{}/source_buffer", data_dir);

        match fs::create_dir(temp_dir) {
            Ok(_g) => {}
            Err(_err) => {}
        }

        let s3_client = Client::from_conf(s3_config);

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

        DataSourceS3Plugin {
            s3_client,
            ingest: Ingest::new(),
            config,
            temp_dir: temp_dir.to_string(),
        }
    }

    pub async fn sync(
        &mut self,
        offsets: Arc<Offsets>,
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

        let mut continuation_token: Option<String> = None;

        let chunk_size = self.config.batch_size_bytes.clone().unwrap_or(10000000);

        let mut i = 0;
        let mut chunk_size_current = 0;

        let mut s3_prefix = inventory_prefix.trim_start_matches('/').to_string();

        if s3_prefix == "/".to_string() || s3_prefix == "./".to_string() {
            s3_prefix = "".to_string();
        }

        let mut list_obj_req = self
            .s3_client
            .list_objects_v2()
            .bucket(s3_bucket.clone())
            .prefix(s3_prefix.clone())
            .max_keys(10000);


        let max_empty_objects = 100;
        let mut empty_objects = 0;

        loop {
            match list_obj_req.clone().send().await {

                Err(err) => {
                    println!("S3 Error: {:?}", err);
                },
                Ok(output) => {
                    let objects = match output.contents() {
                        Some(objects) => {
                            empty_objects = 0;
                            objects
                        },
                        None => {
                            if empty_objects >= max_empty_objects {
                                println!("No objects found in S3, skipping Bucket: {} Prefix: {}", s3_bucket, s3_prefix);
                                break;
                            }
                            empty_objects += 1;
                            continue;
                        }
                    };

                    if !objects.is_empty() {
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
                                        &offsets_clone
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
                            // if skipped_objects >= 100000 {
                            //     println!("Skipped {} objects... already processed", skipped_objects);
                            // }
                            skipped_objects = 0;
                        }
                    }

                    if output.clone().next_continuation_token.is_some() {
                        continuation_token = output.clone().next_continuation_token;

                        // println!(
                        //     "Listing with next continuation token {}",
                        //     continuation_token.clone().unwrap()
                        // );

                        list_obj_req =
                            list_obj_req.set_continuation_token(continuation_token.clone());
                    } else {
                        println!("Reached end of S3 pagination");

                        if !outputs.is_empty() {
                            self.download_and_ingest(
                                &s3_bucket,
                                &outputs,
                                &offsets_clone,
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
                        println!("Successful retry of object {}", key);
                    }
                    return Ok(result);
                }
                Err(err) => {

                    retries += 1;

                    backoff_duration *= 2;

                    println!(
                        "Failed to get object {}, retry {} of {} in {} seconds: {}",
                        key,
                        retries,
                        max_retries,
                        backoff_duration.as_secs(),
                        err.to_string()
                    );

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
        self.ingest.ingest_file(batch, &offsets_clone);
    }

}

struct Download {
    key: String,
    response: GetObjectOutput,
}
