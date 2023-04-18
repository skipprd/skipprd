use crate::helpers::configuration::Config;
use crate::helpers::Helpers;
use crate::serdes::json::SerdeJson;
pub use aws_smithy_http::byte_stream::AggregatedBytes;
use aws_sdk_s3::{Client};
use aws_types::SdkConfig;
use csv::ReaderBuilder;
use flate2::read::GzDecoder;
use regex::internal::Input;
use std::any::Any;
use std::collections::HashMap;
use std::fs::File;
use std::future::Future;
use std::io::{BufRead, BufReader, BufWriter, Cursor, Read, Seek, Write};
use std::path::Path;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::{fs, thread};
use std::time::Duration;
use futures::future::join_all;
use crate::buffer::BufferChunker;


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

        let data_dir= Config::get_data_dir();
        let temp_dir = &format!("{}", data_dir);

        match fs::create_dir(format!("{}/source_buffer", temp_dir)) {
            Ok(g) => {},
            Err(_err) => {}
        }

        let s3_client = Client::new(&s3_config);

        DataSourceS3InventoryPlugin {
            s3_client,
            source_bucket: String::new(),
            temp_dir: temp_dir.to_string(),
        }
    }

    pub async fn sync(&mut self) {

        let mut outputs = Vec::new();

        let inventory_bucket = Config::getenv("S3_BUCKET", "");
        let inventory_prefix = Config::getenv("S3_PREFIX", "");

        println!("Syncing inventory from bucket: {} and prefix {}", inventory_bucket.clone(), inventory_prefix);
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
            Err(err) => println!("S3 Error {}", err),
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

                                // println!("Processing S3 manifest {}", object_key);

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
                                                self.temp_dir.to_string() + "/s3-inventory-temp.csv.gz",
                                            )
                                            .unwrap();

                                            tmpfile.write_all(&tmp_file_content);

                                            let file = File::open(
                                                self.temp_dir.to_string() + "/s3-inventory-temp.csv.gz",
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

                                            // let records_total = rdr.records().count();

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

                                                outputs.push(self.get_object(target_bucket, urldecode::decode(target_key)));

                                                i += 1;

                                                // println!("{} of {}", i, records_total);

                                                if i > 60 {
                                                    join_all(outputs).await;
                                                    outputs = Vec::new();
                                                    i = 0;
                                                }
                                                // else if i >= records_total {
                                                //     join_all(outputs).await;
                                                //     outputs = Vec::new();
                                                //     i = 0;
                                                // }
                                            }
                                            join_all(outputs).await;
                                            outputs = Vec::new();
                                            i = 0;
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

        // true
    }

    pub async fn get_object(&self, source_bucket: String, key: String) {

        // println!("Async getting {}", &key);

        let data = self
            .s3_client
            .get_object()
            .bucket(&source_bucket)
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

        let file_name = BufferChunker::encode_chunk_name("source_buffer", Some(&source_bucket), None, None);

        // let out_filename = self.temp_dir.to_string() + "/source_buffer/" + &Helpers::random_str(10);
        let out_filename = self.temp_dir.to_string() + "/source_buffer/" + &file_name + "-" + &Helpers::random_str(10);


        // A dummy output
        let mut out_file = File::create(out_filename).unwrap();

        if key.contains(".gz") {

            // Something that implements `std::io::Read`
            let c = Cursor::new(data);

            // To inflate on the fly, "pipe" the data through the decoder, i.e. wrap the reader
            let mut stream = GzDecoder::new(c);

            // Consume the `Read`er somehow
            std::io::copy(&mut stream, &mut out_file).unwrap();

        } else {

            let mut c = Cursor::new(data);

            // let mut stream = BufReader::new(data);

            // Consume the `Read`er somehow
            std::io::copy(&mut c, &mut out_file).unwrap();

        }
        // Using the raw data would look like this:
        // std::io::copy(&mut c, &mut out_file).unwrap();





        // let mut buf: Vec<u8> = vec![0];
        // stream.read_to_end(&mut buf);
        // buf

        // let mut tmpfile =
        //     File::create(self.temp_dir.to_string() + "/skippr/s3-" + &Helpers::random_str(10)).unwrap();
        // tmpfile.write_all(&tmp_file_content);

        // println!("Downloaded {}", &key);
    }
}
