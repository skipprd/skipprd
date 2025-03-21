use crate::buffer::BufferChunker;


use crate::helpers::configuration::Config;

use crate::helpers::Helpers;

use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::{Client as S3Client, Error};
use chrono::prelude::*;


use std::fs;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

pub struct DataOutputS3Plugin {
    s3_client: S3Client,
    s3_bucket: String,
    s3_prefix: String,
    time_bucket: String,
    buffer_name: String
}

const GRANULARITIES: [&str; 5] = ["year", "month", "day", "hour", "minute"];

impl DataOutputS3Plugin {
    pub async fn new(buffer_name: String) -> DataOutputS3Plugin {
        let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest()).load().await;

        let s3_client = S3Client::new(&aws_config);

        let s3_bucket = Config::getenv("DATA_OUTPUT_S3_BUCKET", "");
        let s3_prefix = Config::getenv("DATA_OUTPUT_S3_PREFIX", "");
        let time_bucket = Config::getenv("TRANSFORM_BATCH_TIME_UNIT", "");

        Self {
            s3_client,
            s3_bucket,
            s3_prefix,
            time_bucket,
            buffer_name: buffer_name
        }
    }

    pub async fn sync(&self) {
        let _partition_cache: Vec<String> = vec![];

        while let Some(filename) = BufferChunker::next_file(&self.buffer_name) {
            let mut file = BufReader::new(File::open(&filename).unwrap());

            let mut contents = Vec::new();
            file.read_to_end(&mut contents).unwrap();

            let _bucket = &self.s3_bucket;
            let key = &self.s3_prefix;

            let namespace = BufferChunker::decode_file_namespace(&filename);
            // let _time_partition = BufferChunker::decode_file_time(&filename);

            let trimmed_key = &key.trim_matches('/').to_string();

            let mut full_key = "".to_string();
            if !namespace.is_empty() {
                if !trimmed_key.is_empty() {
                    full_key = format!("{}/{}", trimmed_key, namespace);
                } else {
                    full_key = format!("{}", namespace);
                }
            }

            let partition_path = BufferChunker::decode_file_partition(&filename);

            if !partition_path.is_empty() {

                full_key = format!("{}/{}", full_key, partition_path);
            }

            let time_partition_str = BufferChunker::decode_file_time_to_datetime_string(&filename);

            if !time_partition_str.is_empty() {
                let granularity_target = &self.time_bucket;

                let date = DateTime::parse_from_rfc3339(&time_partition_str).unwrap();

                for granularity in GRANULARITIES.iter() {
                    let foo: u32 = match granularity {
                        &"year" => date.year() as u32,
                        &"month" => date.month(),
                        &"day" => date.day(),
                        &"hour" => date.hour(),
                        &"minute" => date.minute(),
                        _ => {
                            panic!("Did not recognise date granularity of {}", granularity);
                        }
                    };

                    full_key = format!("{}/{}={}", full_key, granularity, foo);

                    if granularity == &granularity_target {
                        break;
                    }
                }
            }

            let final_key = format!("{}/{}", full_key, Helpers::random_password(32));

            DataOutputS3Plugin::upload_object(
                &self.s3_client,
                &self.s3_bucket,
                &final_key,
                &filename,
            )
            .await
            .unwrap();
        }
    }

    async fn upload_object(
        client: &S3Client,
        bucket: &str,
        key: &str,
        filename: &str,
    ) -> Result<(), Error> {

        let body = ByteStream::from_path(Path::new(filename)).await;

        match body {
            Ok(b) => {
                // println!("Uploading file: {} to Bucket: {} and Prefix: {}", filename, bucket, key);

                match client
                    .put_object()
                    .bucket(bucket)
                    .key(key)
                    .body(b)
                    .send()
                    .await
                {
                    Ok(_resp) => {
                        println!("Uploaded to S3: {}", key);
                        match fs::remove_file(Path::new(&filename)) {
                            Ok(_) => {}
                            Err(_) => {
                                // @todo - log this back to skippr platform
                            }
                        };
                    }
                    Err(err) => {
                        println!("Failed to upload file: {} to bucket {}, will retry later.", filename, bucket);
                        println!("{}", err);

                        // LOGGER
                        //     .write()
                        //     .await
                        //     .log(
                        //         LogLevel::Error,
                        //         format!(
                        //         "Athena Plugin failed to upload file: {}, key: {} with error: {:?}",
                        //         filename,
                        //         key,
                        //         err.into_service_error()
                        //     ),
                        //     )
                        //     .await;
                    }
                }

                // let resp = client.get_object().bucket(bucket).key(key).send().await?;
                // println!("Response: {:?}", resp);

                // let data = resp.body.collect().await;
                // println!("data: {:?}", data.unwrap().into_bytes());
            }
            Err(e) => {
                println!("Failed to read file before uploading: {}, will retry later.", filename);
                println!("{}", e);
                // println!("{}", e);
            }
        }

        Ok(())
    }
}
