use crate::helpers::configuration::{Config, PluginConfig};

use crate::serdes::json::SerdeJson;
use aws_sdk_s3::Client;

pub use aws_smithy_http::byte_stream::AggregatedBytes;
use csv::ReaderBuilder;
use flate2::read::GzDecoder;

use std::collections::HashMap;
use std::fs::File;

use std::io::{BufRead, BufReader, Cursor, Read, Write};

use std::future::Future;
use std::sync::{Arc};

use aws_sdk_s3::operation::get_object::{GetObjectError, GetObjectOutput};


use std::sync::atomic::Ordering;

use std::time::Duration;
use std::{fs, thread};


use futures::future::join_all;
use futures::StreamExt;

// use crate::thread_pool::ThreadPool;

use crate::helpers::offsets::{OffsetKey, OffsetTypes, Offsets};
use crate::ingest_work::{Ingest, IngestBatch};
use crate::{RUNNING};
use std::sync::RwLock;
use serde_derive::Deserialize;
use tokio::sync::Semaphore;
use crate::helpers::configuration::PluginConfig::s3_inventory;


#[derive(Debug, Deserialize, Clone)]
pub struct DataSourceS3InventoryPluginConfig {
    pub format: Option<String>,
    pub batch_size_seconds: Option<i64>,
    pub batch_size_bytes: Option<i64>,

    pub s3_bucket: String,
    pub s3_prefix: String,

}

impl From<PluginConfig> for DataSourceS3InventoryPluginConfig {
    fn from(plugin_config: PluginConfig) -> Self {
        match plugin_config {
            PluginConfig::s3_inventory(s3_inventory_config) => s3_inventory_config,
            _ => panic!("Invalid plugin type"),
        }
    }
}

impl Into<PluginConfig> for DataSourceS3InventoryPluginConfig {
    fn into(self) -> PluginConfig {
        PluginConfig::s3_inventory(self)
    }
}

pub struct DataSourceS3InventoryPlugin {
    // config: HashMap<String, String>,
    // buffer: Sender<String>,
    s3_client: Client,
    ingest: Ingest,
    config: DataSourceS3InventoryPluginConfig,
    temp_dir: String,
}

impl DataSourceS3InventoryPlugin {
    // pub async fn new(config: HashMap<String, String>, buffer: Sender<String>) -> DataSourceS3InventoryPlugin {
    pub async fn new() -> DataSourceS3InventoryPlugin {
        let s3_config = aws_config::from_env().load().await;

        let data_dir = Config::get_data_dir();
        let temp_dir = &format!("{}/source_buffer", data_dir);

        match fs::create_dir(temp_dir) {
            Ok(_g) => {}
            Err(_err) => {}
        }

        let s3_client = Client::new(&s3_config);

        let config: DataSourceS3InventoryPluginConfig = match Config::get_pipline_plugin_config("input") {
            Ok(config) => config.into(),
            Err(_) => DataSourceS3InventoryPluginConfig {
                format: None,
                batch_size_seconds: Some(Config::getenv("DATA_SOURCE_BATCH_SIZE_SECONDS", "600").parse::<i64>().unwrap()),
                batch_size_bytes: Some(Config::getenv("DATA_SOURCE_BATCH_SIZE_BYTES", "1024000").parse::<i64>().unwrap()),
                s3_bucket: Config::getenv("DATA_SOURCE_S3_INVENTORY_BUCKET", ""),
                s3_prefix: Config::getenv("DATA_SOURCE_S3_INVENTORY_PREFIX", ""),
            }
        };

        DataSourceS3InventoryPlugin {
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

        let inventory_bucket = self.config.s3_bucket.clone();
        let inventory_prefix = self.config.s3_prefix.clone();

        println!(
            "Syncing inventory from bucket: {} and prefix {}",
            inventory_bucket.clone(),
            inventory_prefix
        );
        // let mut params = vec![
        //     ("Bucket".to_string(), inventory_bucket.to_string()),
        // ];
        //
        // if let Some(s3_inventory_prefix) = Config::getenv("s3_inventory_prefix", "") {
        //     params.push(("Prefix".to_string(), s3_inventory_prefix.to_string()));
        // }

        let mut inventory_prefix = inventory_prefix.trim_matches('/').to_string();

        if inventory_prefix == "/".to_string() || inventory_prefix == "./".to_string() {
            inventory_prefix = "".to_string();
        }

        let mut skipped_manifests = 0;

        let results = self
            .s3_client
            .list_objects()
            .bucket(inventory_bucket.clone())
            .prefix(inventory_prefix)
            .send()
            .await;

        match results {
            Err(err) => println!("S3 Error: {}", err.to_string()),
            Ok(..) => {
                for result in results {
                    let objects = match result.contents() {
                        Some(objs) => objs,
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

                            if object_key.contains("manifest.json") {
                                // println!("Processing S3 manifest {}", object_key);

                                let offset_key = OffsetKey {
                                    namespace: inventory_bucket.clone(),
                                    partition: object_key.to_string(),
                                };

                                let has_offsets = offsets_clone.validate(
                                    &offset_key,
                                    OffsetTypes::Closed,
                                    1,
                                );

                                if Some(true) != has_offsets {
                                    let inventory_manifest = self
                                        .s3_client
                                        .get_object()
                                        .bucket(inventory_bucket.clone())
                                        .key(urldecode::decode(object_key.to_string()))
                                        .send()
                                        .await;

                                    // SkipprLogger::debug("Manifest json $inventoryManifest");

                                    let object_content: AggregatedBytes =
                                        inventory_manifest.unwrap().body.collect().await.unwrap();
                                    let manifest = SerdeJson::deserialize(
                                        &String::from_utf8(object_content.into_bytes().to_vec())
                                            .unwrap(),
                                    );

                                    let source_bucket =
                                        manifest.first().unwrap()["sourceBucket"].to_string();
                                    let headers_string: String =
                                        manifest.first().unwrap()["fileSchema"].to_string();
                                    let headers: Vec<_> = headers_string.split(',').collect();

                                    let _bucket = source_bucket.to_string();

                                    let chunk_size = self.config.batch_size_bytes.clone().unwrap_or(10000000);

                                    // let chunk_size =
                                    //     Config::getenv("DATA_SOURCE_BATCH_SIZE_BYTES", "1024000")
                                    //         .parse::<i64>()
                                    //         .unwrap();

                                    for file in manifest.first().unwrap()["files"].as_array() {
                                        let file_key =
                                            file.first().unwrap()["key"].as_str().unwrap();

                                        println!(
                                            "Loading {} manifest from bucket {}",
                                            file_key,
                                            inventory_bucket.clone()
                                        );

                                        let tmp_file = self
                                            .s3_client
                                            .get_object()
                                            .bucket(inventory_bucket.clone())
                                            .key(urldecode::decode(file_key.to_string()))
                                            .send()
                                            .await;

                                        match tmp_file {
                                            Err(..) => println!(
                                                "Get Object Error: {}",
                                                String::from_utf8(
                                                    tmp_file
                                                        .unwrap()
                                                        .body
                                                        .collect()
                                                        .await
                                                        .unwrap()
                                                        .into_bytes()
                                                        .to_vec()
                                                )
                                                .unwrap()
                                            ),
                                            Ok(..) => {
                                                let tmp_file_content = tmp_file
                                                    .unwrap()
                                                    .body
                                                    .collect()
                                                    .await
                                                    .unwrap()
                                                    .into_bytes()
                                                    .to_vec();

                                                let mut tmpfile = File::create(
                                                    self.temp_dir.to_string()
                                                        + "/s3-inventory-temp.csv.gz",
                                                )
                                                .unwrap();

                                                tmpfile.write_all(&tmp_file_content);

                                                let file = File::open(
                                                    self.temp_dir.to_string()
                                                        + "/s3-inventory-temp.csv.gz",
                                                )
                                                .unwrap();
                                                let file = BufReader::new(file);
                                                let mut file = GzDecoder::new(file);
                                                let mut bytes = Vec::new();
                                                let _con = file.read_to_end(&mut bytes).unwrap();

                                                let mut rdr = ReaderBuilder::new()
                                                    .delimiter(b',')
                                                    .double_quote(true)
                                                    .from_reader(bytes.as_slice());

                                                // let mut j = 0;
                                                // let mut c = 0;
                                                //
                                                // let inventorys = vec![];

                                                let mut i = 0;
                                                let mut chunk_size_current = 0;

                                                // let records_total = rdr.records().count();

                                                let _datas: Vec<IngestBatch> = Vec::new();

                                                while let Some(result) = rdr.records().next() {
                                                    let record = result.unwrap();

                                                    let mut inventory: HashMap<String, String> =
                                                        HashMap::new();

                                                    for i in 0..record.len() {
                                                        // println!("Header {}: {}", headers[i].to_string().trim(), record.get(i).unwrap());

                                                        // if headers[i].to_string().trim().eq(&"Key".to_string()) {
                                                        //     println!("MATCH {}: {}", headers[i], record.get(i).unwrap());
                                                        // }
                                                        inventory.insert(
                                                            headers[i]
                                                                .to_string()
                                                                .trim()
                                                                .to_string(),
                                                            record.get(i).unwrap().to_string(),
                                                        );
                                                    }

                                                    let target_key =
                                                        inventory.get("Key").unwrap().to_string();
                                                    let target_bucket = inventory
                                                        .get("\"Bucket")
                                                        .unwrap()
                                                        .to_string();

                                                    let offset_key = OffsetKey {
                                                        namespace: target_bucket.clone(),
                                                        partition: target_key.clone(),
                                                    };
                                                    if Some(true)
                                                        != offsets_clone.validate(
                                                            &offset_key,
                                                            OffsetTypes::Closed,
                                                            1,
                                                        )
                                                    {
                                                        outputs.push(target_key);

                                                        chunk_size_current += match inventory
                                                            .get("Size")
                                                            .unwrap()
                                                            .parse::<i64>()
                                                        {
                                                            Ok(size) => size,
                                                            Err(_err) => 0,
                                                        };

                                                        i += 1;

                                                        if chunk_size_current >= chunk_size {
                                                            self.download_and_ingest(
                                                                &target_bucket,
                                                                &outputs,
                                                                &offsets_clone,
                                                            )
                                                            .await;

                                                            outputs = Vec::new();
                                                            i = 0;
                                                            chunk_size_current = 0;
                                                        }
                                                    } else {
                                                        // println!("Skipping object: {} already processed", target_key);
                                                    }
                                                }
                                            }
                                        };
                                    }
                                } else {
                                    skipped_manifests += 1;

                                }

                                offsets.set(
                                    &OffsetKey {
                                        namespace: inventory_bucket.clone(),
                                        partition: object_key.to_string(),
                                    },
                                    OffsetTypes::Closed,
                                    1,
                                );
                            }
                        }

                        if skipped_manifests > 0 {
                            println!(
                                "Skipped {} inventory manifests... already processed",
                                skipped_manifests
                            );
                            skipped_manifests = 0;
                        }

                    } else {
                        println!("Reached end of S3 pagination");

                        if !outputs.is_empty() {
                            self.download_and_ingest(
                                &inventory_bucket,
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

                    // let wait_time = backoff_duration.as_secs_f64() * 2.0_f64.powi(retries);
                    thread::sleep(Duration::from_secs_f64(backoff_duration.as_secs_f64()));

                    backoff_duration *= 2;

                    println!(
                        "Failed to get object {}, retry {} of {} in {} seconds: {}",
                        key,
                        retries,
                        max_retries,
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

        let datas: Arc<RwLock<Vec<IngestBatch>>> = Arc::new(RwLock::new(Vec::new()));

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

                            datas_clone.write().unwrap().push(IngestBatch {
                                offset_key: OffsetKey {
                                    namespace: bucket_name_clone.to_string(),
                                    partition: download.key,
                                },
                                data: decompressed_data,
                            });
                        } else {
                            let str_data = String::from_utf8(data_vec).unwrap();

                            datas_clone.write().unwrap().push(IngestBatch {
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

        let batch = datas.read().unwrap().to_vec();
        self.ingest.ingest_file(batch, &offsets_clone);
    }
}

struct Download {
    key: String,
    response: GetObjectOutput,
}
