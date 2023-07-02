use crate::helpers::configuration::{Config};

use aws_sdk_s3::Client;
use aws_sdk_s3::Error as S3Error;
pub use aws_smithy_http::byte_stream::AggregatedBytes;

use flate2::read::GzDecoder;
use regex::internal::Input;

use std::collections::HashMap;

use std::io::{Cursor, Read};

use std::future::Future;
use std::sync::{Arc, Mutex, RwLock};

use aws_sdk_s3::operation::get_object::{GetObjectError, GetObjectOutput};
use aws_smithy_http::result::SdkError;
use std::sync::atomic::Ordering;
use std::thread::sleep;
use std::time::Duration;
use std::{fs, thread};

use crate::discover::Metadata;
use futures::future::join_all;
use futures::{AsyncReadExt, StreamExt};

// use crate::thread_pool::ThreadPool;

use crate::helpers::offsets::{OffsetKey, OffsetTypes, Offsets};
use crate::ingest_work::{Ingest, IngestBatch};
use rusoto_core::{Region, RusotoError};
use tokio::sync::Semaphore;
// use rusoto_s3::{GetObjectOutput, GetObjectRequest, ListObjectsV2Request, S3Client, S3};

use crate::{INPUT_GRACEFUL_SHUTDOWN_COMPLETE, RUNNING};
use tokio::time::timeout;

pub struct DataSourceS3Plugin {
    // config: HashMap<String, String>,
    // buffer: Sender<String>,
    s3_client: Client,
    // s3_client_rusoto: S3Client,
    ingest: Ingest,
    source_bucket: String,
    temp_dir: String,
}

impl DataSourceS3Plugin {
    // pub async fn new(config: HashMap<String, String>, buffer: Sender<String>) -> DataSourceS3Plugin {
    pub async fn new() -> DataSourceS3Plugin {
        let s3_config = aws_config::from_env().load().await;

        let data_dir = Config::get_data_dir();
        let temp_dir = &format!("{}/source_buffer", data_dir);

        match fs::create_dir(temp_dir) {
            Ok(_g) => {}
            Err(_err) => {}
        }

        let s3_client = Client::new(&s3_config);

        DataSourceS3Plugin {
            s3_client,
            // s3_client_rusoto: S3Client::new(Region::default()),
            ingest: Ingest::new(),
            source_bucket: String::new(),
            temp_dir: temp_dir.to_string(),
        }
    }

    pub async fn sync(
        &mut self,
        // pool: &mut ThreadPool,
        metadata: Arc<Mutex<HashMap<String, Metadata>>>,
        offsets: Arc<Offsets>,
    ) {
        let metadata = metadata.clone();

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

        let inventory_bucket = Config::getenv("DATA_SOURCE_S3_BUCKET", "");
        let mut inventory_prefix = Config::getenv("DATA_SOURCE_S3_PREFIX", "");

        println!(
            "Syncing from bucket: {} and prefix {}",
            inventory_bucket.clone(),
            inventory_prefix
        );

        let mut continuation_token: Option<String> = None;

        let chunk_size = Config::getenv("DATA_SOURCE_BATCH_SIZE_BYTES", "1024000")
            .parse::<i64>()
            .unwrap();

        let mut i = 0;
        let mut chunk_size_current = 0;

        let mut inventory_prefix = inventory_prefix.trim_matches('/').to_string();

        if inventory_prefix == "/".to_string() || inventory_prefix == "./".to_string() {
            inventory_prefix = "".to_string();
        }

        let mut list_obj_req = self
            .s3_client
            .list_objects_v2()
            .bucket(inventory_bucket.clone())
            .prefix(inventory_prefix.clone())
            .max_keys(10000);

        loop {
            match list_obj_req.clone().send().await {
                Err(err) => println!("S3 Error {}", err),
                Ok(output) => {
                    let objects = match output.contents() {
                        Some(objects) => objects,
                        None => {
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
                                namespace: inventory_bucket.clone(),
                                partition: object_key.to_string(),
                            };

                            if Some(true)
                                != offsets_clone.validate(&offset_key, OffsetTypes::Closed, 1)
                            {
                                // println!("getting key: {}", object_key);

                                outputs.push(object_key.to_string());

                                chunk_size_current += object.size();

                                i += 1;

                                // println!("State {} = {} of {}", i, chunk_size_current, chunk_size);

                                if chunk_size_current >= chunk_size {
                                    println!("Proccessing {} Objects, totalling {} bytes (batch size config {} bytes)", i, chunk_size_current, chunk_size);

                                    Self::download_and_ingest(
                                        &mut self.s3_client,
                                        &inventory_bucket,
                                        &outputs,
                                        &self.temp_dir,
                                        &metadata,
                                        &offsets_clone
                                    )
                                    .await;

                                    outputs = Vec::new();
                                    i = 0;
                                    chunk_size_current = 0;
                                }
                            } else {
                                // println!("Skipping object: {} already processed", object_key);
                            }
                        }
                    }

                    if output.clone().next_continuation_token.is_some() {
                        continuation_token = output.clone().next_continuation_token;

                        println!(
                            "Listing with next continuation token {}",
                            continuation_token.clone().unwrap()
                        );

                        list_obj_req =
                            list_obj_req.set_continuation_token(continuation_token.clone());
                    } else {
                        println!("Reached end of S3 pagination");

                        if !outputs.is_empty() {
                            Self::download_and_ingest(
                                &mut self.s3_client,
                                &inventory_bucket,
                                &outputs,
                                &self.temp_dir,
                                &metadata,
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
            let mut get_request = s3_client
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

                    // let wait_time = backoff_duration.as_secs_f64() * 2.0_f64.powi(retries);
                    thread::sleep(Duration::from_secs_f64(backoff_duration.as_secs_f64()));

                    backoff_duration *= 2;

                    println!(
                        "Failed to get object {}, retry back in {} seconds: {}",
                        key,
                        backoff_duration.as_secs(),
                        err.to_string()
                    );

                    if retries >= max_retries {
                        return Err(err.into_service_error());
                    }
                }
            }
        }
    }

    async fn download_and_ingest(
        s3_client: &Client,
        bucket_name: &String,
        object_keys: &Vec<String>,
        _output_dir: &String,
        metadata: &Arc<Mutex<HashMap<String, Metadata>>>,
        offsets_clone: &Arc<Offsets>,
    ) {
        let semaphore = Arc::new(Semaphore::new(2048));

        let futures: Vec<_> = object_keys
            .clone()
            .into_iter()
            .map(|object_key| {
                let semaphore = Arc::clone(&semaphore);
                let s3_client = s3_client.clone();
                let bucket_name = bucket_name.to_owned();

                tokio::spawn(async move {
                    let permit = semaphore.acquire().await.unwrap();

                    let mut _x_fut = s3_client
                        .get_object()
                        .bucket(bucket_name.clone())
                        .key(urldecode::decode(object_key.to_string()));

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

        let datas: Arc<RwLock<Vec<IngestBatch>>> = Arc::new(RwLock::new(Vec::new()));

        let future_result = tokio::join!(join_all(futures)).0;

        let mut threads: Vec<_> = Vec::new();

        let data_dir = Config::get_data_dir();
        let _temp_dir = &format!("{}/source_buffer", data_dir);

        let datas_clone = datas.clone();

        let bucket_name = bucket_name.clone();
        let metadata = metadata.clone();
        let offsets_clone = offsets_clone.clone();
        let bucket_name = bucket_name.clone();
        let datas_clone = datas.clone();

        // threads.push(thread::spawn(move || {

        // let mut batch: RwLock<IngestBatch> = RwLock::new(IngestBatch {
        //     offset_key: OffsetKey {
        //         namespace: bucket_name.to_string(),
        //         partition: "".to_string(),
        //     },
        //     data: "".to_string(),
        // });
        // let mut batch_clone = batch.clone();

        tokio::spawn(async move {
            // for thread in threads {
            for future in future_result {
                match future.unwrap() {
                    Ok(mut download) => {
                        // println!("Downloading s3 object");
                        let mut data = download.response.body;

                        // convert the ByteStream into a Vec<u8>
                        let mut data_vec = Vec::new();
                        while let Some(chunk) = data.next().await {
                            data_vec.extend_from_slice(&chunk.unwrap());
                        }

                        if download.key.contains(".gz") {
                            // Something that implements `std::io::Read`
                            let c = Cursor::new(data_vec);

                            // To inflate on the fly, "pipe" the data through the decoder, i.e. wrap the reader
                            let mut stream = GzDecoder::new(c);

                            let mut decompressed_data = String::new();
                            stream.read_to_string(&mut decompressed_data).unwrap();

                            datas_clone.write().unwrap().push(IngestBatch {
                                offset_key: OffsetKey {
                                    namespace: bucket_name.to_string(),
                                    partition: download.key,
                                },
                                data: decompressed_data,
                            });
                        } else {
                            let str_data = String::from_utf8(data_vec).unwrap();

                            datas_clone.write().unwrap().push(IngestBatch {
                                offset_key: OffsetKey {
                                    namespace: bucket_name.to_string(),
                                    partition: download.key,
                                },
                                data: str_data,
                            });
                        }
                    }
                    Err(err) => {
                        println!("{:?}", err);
                    }
                };
            }
        })
        .await
        .unwrap();

        // datas.lock().unwrap().push(batch.lock().unwrap().clone());
        let datas = datas.clone();

        threads.push(thread::spawn(move || {
            let batch = datas.read().unwrap().to_vec();
            self::Ingest::ingest_file(batch, &metadata, &offsets_clone);
        }));

        // Wait for all threads to finish, else we will stampead the data source
        for handle in threads {
            match handle.join() {
                Ok(_) => {}
                Err(err) => {
                    println!("ERROR: {:#?}", err);
                }
            }
        }

        if !RUNNING.lock().unwrap().load(Ordering::SeqCst) {
            INPUT_GRACEFUL_SHUTDOWN_COMPLETE
                .lock()
                .unwrap()
                .store(true, Ordering::SeqCst);
            // sleep(Duration::from_secs(120));
        }
    }
}

struct Download {
    key: String,
    response: GetObjectOutput,
}
