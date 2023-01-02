use crate::helpers::configuration::Config;
use crate::helpers::Helpers;
use crate::serdes::json::SerderJson;
use aws_sdk_s3::types::AggregatedBytes;
use aws_sdk_s3::{Client, Region};
use aws_types::credentials::ProvideCredentials;
use aws_types::SdkConfig;
use csv::ReaderBuilder;
use flate2::read::GzDecoder;
use regex::internal::Input;
use std::any::Any;
use std::collections::HashMap;
use std::fs::File;
use std::future::Future;
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, Write};
use std::path::Path;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use futures::future::join_all;

pub struct DataSourceS3InventoryPlugin {
    // config: HashMap<String, String>,
    // buffer: Sender<String>,
    s3_client: Client,
    source_bucket: String,
    temp_dir: String,
}

impl DataSourceS3InventoryPlugin {
    // pub async fn new(config: HashMap<String, String>, buffer: Sender<String>) -> DataSourceS3InventoryPlugin {
    pub async fn new() -> DataSourceS3InventoryPlugin {
        let mut s3_config = aws_config::from_env().load().await;
        let temp_dir = "/tmp".to_string();

        // if let Some(s3_region) = config.get("s3_region") {
        // s3_config.region(Region::from_static(s3_region));
        // s3_config.insert("region".to_string(), Region::from_static(s3_region));
        // }
        //
        // if let Some(aws_access_id) = config.get("aws_access_id") {
        // s3_config.insert("credentials".to_string(), aws_access_id.to_string());
        // }
        //
        // if let Some(aws_secret_key) = config.get("aws_secret_key") {
        // s3_config.insert("credentials".to_string(), aws_secret_key.to_string());
        // }
        //
        // if let Some(endpoint) = config.get("endpoint") {
        //     s3_config.set_endpoint_resolver()
        //     s3_config.insert("endpoint".to_string(), endpoint.to_string());
        // }
        //
        // if let Some(role_arn) = config.get("role_arn") {
        //     s3_config.insert("role_arn".to_string(), role_arn.to_string());
        // }
        //
        // if let Some(role_session_name) = config.get("role_session_name") {
        //     s3_config.insert("role_session_name".to_string(), role_session_name.to_string());
        // }

        let s3_client = Client::new(&s3_config);
        // let s3_helpers = AwsS3::new(s3_client);

        DataSourceS3InventoryPlugin {
            // config,
            // buffer,
            s3_client,
            source_bucket: String::new(),
            temp_dir,
        }
    }

    pub async fn sync(&mut self) -> bool {

        let mut outputs = Vec::new();

        let inventory_bucket = Config::getenv("s3_bucket", "");

        println!("Syncing from bucket: {}", inventory_bucket.clone());
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
            .send()
            .await;

        match results {
            Err(..) => println!("S3 Error {}", results.err().unwrap()),
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
                            let timestamp = object.last_modified().unwrap().secs();

                            if object_key.contains(&"manifest.json") {
                                println!("Processing S3 manifest {}", object_key);
                                // SkipprLogger::debug("Processing S3 manifest $objectKey");

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
                                let manifest = SerderJson::deserialize(
                                    String::from_utf8(object_content.into_bytes().to_vec())
                                        .unwrap(),
                                );

                                let mut source_bucket =
                                    manifest.first().unwrap()["sourceBucket"].to_string();
                                let headers_string: String =
                                    manifest.first().unwrap()["fileSchema"].to_string();
                                let headers: Vec<_> = headers_string.split(",").collect();

                                let bucket = source_bucket.to_string();

                                let chunk_size = Config::getenv("DATA_SOURCE_BATCH_SIZE", "10");

                                for file in manifest.first().unwrap()["files"].as_array() {
                                    let file_key = file.first().unwrap()["key"].as_str().unwrap();
                                    println!(
                                        "Loading {} manifest from bucket {}",
                                        file_key,
                                        inventory_bucket.clone()
                                    );
                                    // SkipprLogger::info("Loading {$file['key']} manifest");

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
                                                self.temp_dir.to_string() + "/s3-inventory-temp",
                                            )
                                            .unwrap();
                                            tmpfile.write_all(&tmp_file_content);

                                            let file = File::open(
                                                self.temp_dir.to_string() + "/s3-inventory-temp",
                                            )
                                            .unwrap();
                                            let file = BufReader::new(file);
                                            let mut file = GzDecoder::new(file);
                                            let mut bytes = Vec::new();
                                            let con = file.read_to_end(&mut bytes).unwrap();

                                            let mut rdr = ReaderBuilder::new()
                                                .delimiter(b',')
                                                .double_quote(true)
                                                .from_reader(bytes.as_slice());

                                            // let mut j = 0;
                                            // let mut c = 0;
                                            //
                                            // let inventorys = vec![];

                                            let mut i = 0;

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

                                                // if !inventory.get("Key").unwrap().is_some() {
                                                // inventorys.push(inventory);

                                                // let key = &inventory[&"Key".to_string()];
                                                let target_key = inventory.get("Key").unwrap().to_string();
                                                let target_bucket = inventory.get("\"Bucket").unwrap().to_string();

                                                // println!("Getting {}", target_key);

                                                outputs.push(self.get_object(target_bucket, target_key));

                                                i += 1;

                                                if i > 60 {
                                                    join_all(outputs).await;
                                                    outputs = Vec::new();
                                                    i = 0;
                                                }
                                            }

                                        }
                                    };
                                }
                            }
                        }
                    }
                }
            }
        }

        // join_all(outputs).await;

        true
    }

    pub async fn get_object(&self, source_bucket: String, key: String) {

        // println!("Async getting {}", &key);
        let tmp_file_content = self
            .s3_client
            .get_object()
            .bucket(source_bucket)
            .key(&key)
            .send()
            .await
            .unwrap()
            .body
            .collect()
            .await
            .unwrap()
            .into_bytes()
            .to_vec();

        // println!("Got {}", &key);

        let mut tmpfile =
            File::create(self.temp_dir.to_string() + "/ddd/s3-" + &Helpers::random_str(10)).unwrap();
        tmpfile.write_all(&tmp_file_content);

        // println!("Downloaded {}", &key);
    }
}
