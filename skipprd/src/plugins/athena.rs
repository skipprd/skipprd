use std::fs;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use chrono::prelude::*;
use chrono::prelude::*;
use std::path::PathBuf;
use aws_sdk_athena::{Client as AthenaClient};
use aws_sdk_s3::{Client as S3Client, Error};
use aws_sdk_s3::primitives::ByteStream;
use parquet::data_type::AsBytes;
use crate::buffer::BufferChunker;
use crate::helpers::Helpers;

pub struct DataOutputAwsAthenaPlugin {
    s3_client: S3Client,
    athena_client: AthenaClient,
    // config: Config,
    s3_bucket: String,
    s3_prefix: String,
    time_bucket: String,
}

// struct Config {
    // aws_region: S3Region,
    // aws_access_key: String,
    // aws_secret_key: String,
    // endpoint: Option<String>,
    // role_arn: Option<String>,
    // role_session_name: Option<String>,
    // s3_bucket: String,
    // s3_prefix: String,
    // time_bucket: String,
// }

struct AwsS3 {
    // some fields
}

struct AwsAthena {
    // some fields
}

const GRANULARITIES: [&str; 5] = ["year", "month", "day", "hour", "minute"];

impl DataOutputAwsAthenaPlugin {
    pub async fn new() -> DataOutputAwsAthenaPlugin {
        let mut aws_config = aws_config::from_env().load().await;

        let s3_client = S3Client::new(&aws_config);
        let athena_client = AthenaClient::new(&aws_config);

        let s3_bucket = "".to_string();
        let s3_prefix = "".to_string();
        let time_bucket = "".to_string();

        Self {
            s3_client,
            athena_client,
            s3_bucket,
            s3_prefix,
            time_bucket
        }
    }

    pub async fn sync(&self, serde: &str) {
        let mut next_file = BufferChunker::next_file();
        while let Some(filename) = &next_file {
            let mut file = BufReader::new(File::open(&filename).unwrap());

            let mut contents = Vec::new();
            file.read_to_end(&mut contents).unwrap();

            let bucket = &self.s3_bucket;
            let mut key = &self.s3_prefix;

            let namespace = BufferChunker::decode_file_namespace(&filename);
            let partition = BufferChunker::decode_file_partition(&filename);
            let time_partition = BufferChunker::decode_file_time(&filename);

            let trimmed_key = &key.trim_start_matches("/").to_string();
            // key = trimmed_key;
            let namespace_key = if !namespace.is_empty() {
                format!("{}/{}", trimmed_key, namespace)
            } else {
                key.to_string()
            };
            let partition_key = if !partition.is_empty() {
                format!("{}/{}", trimmed_key, partition)
            } else {
                key.to_string()
            };

            let mut full_key = format!("{}/{}", namespace_key, partition_key);

            if !time_partition != 0 {
                let granularity_target = &self.time_bucket;

                let time_partition_str = BufferChunker::decode_chunk_time(&filename);

                let date = DateTime::parse_from_rfc3339(&time_partition_str).unwrap();

                // let mut partition_params = vec![];
                let mut partition_values = vec![];


                for granularity in GRANULARITIES.iter() {
                    full_key = format!("{}/{}={}", full_key, granularity, date.format(granularity));
                    partition_values.push(format!("{}", date.format(granularity)));

                    if granularity == &granularity_target {
                        break;
                    }
                }

                // @todo
                // $this->athenaClient->glueCreatePartition($namespace, $partitionValues, $key);
            }

            let final_key = format!("{}/{}", full_key, Helpers::random_password(32));
            // key = &format!("{}/{}", key, "");

            // let mut reader = BufReader::new(file);

            DataOutputAwsAthenaPlugin::upload_object(&self.s3_client, &self.s3_bucket, &final_key, &filename);

        }
    }

    // Upload a file to a bucket.
// snippet-start:[s3.rust.s3-helloworld]
    async fn upload_object(
        client: &S3Client,
        bucket: &str,
        key: &str,
        filename: &str
    ) -> Result<(), Error> {
        let resp = client.list_buckets().send().await?;

        for bucket in resp.buckets().unwrap_or_default() {
            println!("bucket: {:?}", bucket.name().unwrap_or_default())
        }

        println!();

        let body = ByteStream::from_path(Path::new(filename)).await;

        match body {
            Ok(b) => {
                let resp = client
                    .put_object()
                    .bucket(bucket)
                    .key(key)
                    .body(b)
                    .send()
                    .await?;

                println!("Upload success. Version: {:?}", resp.version_id);

                let resp = client.get_object().bucket(bucket).key(key).send().await?;
                // println!("Response: {:?}", resp);

                // let data = resp.body.collect().await;
                // println!("data: {:?}", data.unwrap().into_bytes());

                fs::remove_file(Path::new(&filename));

            }
            Err(e) => {
                println!("Got an error uploading object:");
                println!("{}", e);
            }
        }

        Ok(())
    }

}