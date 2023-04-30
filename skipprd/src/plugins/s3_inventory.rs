use crate::helpers::configuration::{Config, Metrics};

use crate::serdes::json::SerdeJson;
use aws_sdk_s3::Client;
pub use aws_smithy_http::byte_stream::AggregatedBytes;

use csv::ReaderBuilder;
use flate2::read::GzDecoder;
use regex::internal::Input;

use std::collections::HashMap;
use std::fs::File;

use std::io::{BufRead, BufReader, Cursor, Read, Write};

use std::future::Future;
use std::sync::{Arc, Mutex};


use std::{fs, thread};



use crate::discover::Metadata;
use futures::future::join_all;
use futures::StreamExt;

// use crate::thread_pool::ThreadPool;




use rusoto_core::Region;
use rusoto_s3::{GetObjectRequest, S3Client, S3, GetObjectOutput};
use crate::helpers::offsets::{OffsetKey, Offsets, OffsetTypes};
use crate::ingest_work::{Ingest, IngestBatch};

pub struct DataSourceS3InventoryPlugin {
    // config: HashMap<String, String>,
    // buffer: Sender<String>,
    s3_client: Client,
    s3_client_rusoto: S3Client,
    ingest: Ingest,
    source_bucket: String,
    temp_dir: String,
}

impl DataSourceS3InventoryPlugin {
    // pub async fn new(config: HashMap<String, String>, buffer: Sender<String>) -> DataSourceS3InventoryPlugin {
    pub async fn new() -> DataSourceS3InventoryPlugin {
        let s3_config = aws_config::from_env().load().await;

        let data_dir = Config::get_data_dir();
        let temp_dir = &format!("{}/source_buffer", data_dir);

        match fs::create_dir(format!("{}", temp_dir)) {
            Ok(_g) => {}
            Err(_err) => {}
        }

        let s3_client = Client::new(&s3_config);

        DataSourceS3InventoryPlugin {
            s3_client,
            s3_client_rusoto: S3Client::new(Region::default()),
            ingest: Ingest::new(),
            source_bucket: String::new(),
            temp_dir: temp_dir.to_string(),
        }
    }

    pub async fn sync(
        &mut self,
        // pool: &mut ThreadPool,
        metadata: Arc<Mutex<HashMap<String, Metadata>>>,
        metrics: Arc<Mutex<Metrics>>,
    ) {
        let metadata = metadata.clone();
        let metrics = metrics.clone();

        let offsets =  Arc::new(Offsets::init().unwrap());

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

        let inventory_bucket = Config::getenv("S3_BUCKET", "");
        let inventory_prefix = Config::getenv("S3_PREFIX", "");

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

        let results = self
            .s3_client
            .list_objects()
            .bucket(inventory_bucket.clone())
            .prefix(inventory_prefix)
            .send()
            .await;

        match results {
            Err(err) => println!("S3 Error {}", err.into_service_error()),
            Ok(..) => {
                for result in results {
                    let objects = result.contents().unwrap();

                    if !objects.is_empty() {
                        for object in objects {
                            // if object.is_empty() || object == null {
                            //     SkipprLogger::debug("object has no key");
                            //     continue;
                            // }

                            let object_key = object.key().unwrap();
                            let _timestamp = object.last_modified().unwrap().secs();

                            if object_key.contains(&"manifest.json") {
                                // println!("Processing S3 manifest {}", object_key);

                                let offset_key = OffsetKey { namespace: inventory_bucket.clone(), partition: object_key.to_string() };
                                if Some(true) != offsets_clone.validate(&offset_key, OffsetTypes::Closed, 1) {
                                    let inventory_manifest = self
                                        .s3_client
                                        .get_object()
                                        .bucket(inventory_bucket.clone())
                                        .key(object_key)
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
                                    let headers: Vec<_> = headers_string.split(",").collect();

                                    let _bucket = source_bucket.to_string();

                                    let chunk_size = Config::getenv("DATA_SOURCE_BATCH_SIZE", "10").parse::<i32>().unwrap();

                                    for file in manifest.first().unwrap()["files"].as_array() {
                                        let file_key = file.first().unwrap()["key"].as_str().unwrap();

                                            println!(
                                                "Loading {} manifest from bucket {}",
                                                file_key,
                                                inventory_bucket.clone()
                                            );

                                            let tmp_file = self
                                                .s3_client
                                                .get_object()
                                                .bucket(inventory_bucket.clone())
                                                .key(file_key)
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

                                                    // let records_total = rdr.records().count();

                                                    let mut datas: Vec<IngestBatch> = Vec::new();

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
                                                                headers[i].to_string().trim().to_string(),
                                                                record.get(i).unwrap().to_string(),
                                                            );
                                                        }

                                                        let target_key =
                                                            inventory.get("Key").unwrap().to_string();
                                                        let target_bucket =
                                                            inventory.get("\"Bucket").unwrap().to_string();

                                                        let offset_key = OffsetKey { namespace: target_bucket.clone(), partition: target_key.clone() };
                                                        if Some(true) != offsets_clone.validate(&offset_key, OffsetTypes::Closed, 1) {



                                                            // if !inventory.get("Key").unwrap().is_some() {
                                                            // inventorys.push(inventory);

                                                            // let key = &inventory[&"Key".to_string()];


                                                            outputs.push(target_key);

                                                            i += 1;

                                                            if i >= chunk_size {
                                                                datas = Self::download_and_ingest(
                                                                    &mut self.s3_client_rusoto,
                                                                    &target_bucket,
                                                                    &outputs,
                                                                    &self.temp_dir,
                                                                    // &metadata,
                                                                    // &metrics
                                                                ).await;
                                                                // .expect("failed getting objects");

                                                                // if !datas. {
                                                                self::Ingest::ingest_file(
                                                                    datas,
                                                                    // pool,
                                                                    &metadata,
                                                                    &metrics,
                                                                    &offsets_clone
                                                                );
                                                                // }

                                                                outputs = Vec::new();
                                                                i = 0;
                                                            }
                                                        } else {
                                                            // println!("Skipping object: {} already processed", file_key);
                                                        }
                                                    }
                                                }
                                            };


                                    }

                                    offsets.set(&OffsetKey { namespace: inventory_bucket.clone(), partition: object_key.to_string()}, OffsetTypes::Closed, 1);


                                } else {
                                    println!("Skipping inventory: {} already processed", object_key.to_string());

                                }
                            }
                        }
                    }
                }
            }
        }

        // join_all(outputs).await;

        // true
    }




    async fn download_and_ingest(
        s3_client: &mut S3Client,
        bucket_name: &String,
        object_keys: &Vec<String>,
        _output_dir: &String,
    ) -> Vec<IngestBatch> {
        let threads: Vec<_> = object_keys.clone()
            .into_iter()
            .map(|object_key| {
                let s3_client = s3_client.clone();
                let bucket_name = bucket_name.to_owned();

                tokio::spawn(async move {
                    let x_fut = s3_client
                        .get_object(GetObjectRequest {
                            bucket: bucket_name.clone(),
                            key: object_key.to_string(),
                            ..Default::default()
                        });

                    let response = x_fut.await.expect(&format!("Failed getting object {}", object_key));
                    // println!("got object {}", object_key);

                    let download = Download {
                        key: object_key,
                        response: response,
                    };

                    download
                })
            })
            .collect();


        let datas: Arc<Mutex<Vec<IngestBatch>>> = Arc::new(Mutex::new(Vec::new()));

        let foo = tokio::join!(join_all(threads)).0;

        // for thread in threads {
        for thread in foo {
            let mut download = thread.unwrap();

            let datas = datas.clone();

            let bucket_name = bucket_name.clone();

            thread::spawn(move || {
                let data_dir = Config::get_data_dir();
                let _temp_dir = &format!("{}/source_buffer", data_dir);

                let mut data = Vec::new();
                let _body = download.response.body.take().unwrap().into_blocking_read().read_to_end(&mut data);


                if download.key.contains(".gz") {
                    // Something that implements `std::io::Read`
                    let c = Cursor::new(data);

                    // To inflate on the fly, "pipe" the data through the decoder, i.e. wrap the reader
                    let mut stream = GzDecoder::new(c);

                    // Consume the `Read`er somehow
                    // std::io::copy(&mut stream, &mut file).unwrap();

                    let mut decompressed_data = String::new();
                    stream.read_to_string(&mut decompressed_data).unwrap();

                    datas.lock().unwrap().push(IngestBatch {
                        offset_key: OffsetKey {
                            namespace: bucket_name.to_string(),
                            partition: download.key,
                        },
                        data: decompressed_data,
                    });
                } else {
                    let mut c = Cursor::new(data);

                    // let mut stream = BufReader::new(data);

                    // Consume the `Read`er somehow
                    // std::io::copy(&mut c, &mut file).unwrap();

                    let mut str_data = String::new();
                    c.read_to_string(&mut str_data).unwrap();

                    datas.lock().unwrap().push(IngestBatch {
                        offset_key: OffsetKey {
                            namespace: bucket_name.to_string(),
                            partition: download.key,
                        },
                        data: str_data,
                    });
                }
            }).join().unwrap();
        }

        let ingest_batches: Vec<IngestBatch> = datas.lock().unwrap().to_vec();

        ingest_batches
    }
}

struct Download {
    key: String,
    response: GetObjectOutput
}