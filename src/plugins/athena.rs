use crate::buffer::BufferChunker;
use crate::converters::skippr_hive::SkipprHive;
use crate::discover::Metadata;
use crate::helpers::configuration::{Config, PluginConfig};
use crate::helpers::Helpers;
use crate::{discover, flatten_metadata, METADATA};
use aws_sdk_athena::types::{
    EncryptionConfiguration, EncryptionOption, ResultConfiguration, Tag, WorkGroupConfiguration,
};
use aws_sdk_athena::Client as AthenaClient;
use aws_sdk_glue::types::{
    Column, DatabaseInput, PartitionIndex, PartitionInput, SerDeInfo, StorageDescriptor, TableInput,
};
use aws_sdk_glue::Client as GlueClient;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::{Client as S3Client, Client, Error};
use chrono::prelude::*;

use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use serde::Deserializer;
use serde_derive::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct DataOutputAwsAthenaPluginConfig {
    pub format: Option<String>,
    // pub batch_size_seconds: Option<i64>,
    // pub batch_size_bytes: Option<i64>,

    pub s3_bucket: String,
    pub s3_prefix: String,
    // pub time_bucket: Option<String>,
    pub athena_workgroup_name: String,
    pub glue_database_name: String,

}

impl From<PluginConfig> for DataOutputAwsAthenaPluginConfig {
    fn from(plugin_config: PluginConfig) -> Self {
        match plugin_config {
            PluginConfig::athena(athena_config) => athena_config,
            _ => panic!("Invalid plugin type"),
        }
    }
}

pub struct DataOutputAwsAthenaPlugin {
    s3_client: S3Client,
    athena_client: AthenaClient,
    buffer_name: String,
    config: DataOutputAwsAthenaPluginConfig,
    // s3_bucket: String,
    // s3_prefix: String,
    // time_bucket: String,
}

const GRANULARITIES: [&str; 5] = ["year", "month", "day", "hour", "minute"];

impl DataOutputAwsAthenaPlugin {
    pub async fn new(buffer_name: String) -> DataOutputAwsAthenaPlugin {
        let aws_config = aws_config::from_env().load().await;

        let s3_client = S3Client::new(&aws_config);
        let athena_client = AthenaClient::new(&aws_config);

        // let s3_bucket = Config::getenv("DATA_OUTPUT_S3_BUCKET", "");
        // let s3_prefix = Config::getenv("DATA_OUTPUT_S3_PREFIX", "");
        // let time_bucket = Config::getenv("TRANSFORM_BATCH_TIME_UNIT", "");


        // let athena_config: DataOutputAwsAthenaPluginConfig = Config::get_pipline_plugin_config("output").unwrap().into();
        let athena_config: DataOutputAwsAthenaPluginConfig = match Config::get_pipline_plugin_config("output") {
            Ok(config) => config.into(),
            Err(_) => DataOutputAwsAthenaPluginConfig {
                format: None,
                s3_bucket: Config::getenv("DATA_OUTPUT_S3_BUCKET", ""),
                s3_prefix: Config::getenv("DATA_OUTPUT_S3_PREFIX", ""),
                athena_workgroup_name: Config::getenv("DATA_OUTPUT_ATHENA_WORKGROUP_NAME", ""),
                glue_database_name: Config::getenv("SCHEMA_OUTPUT_GLUE_DATABASE_NAME", ""),
            }
        };

        Self {
            s3_client,
            athena_client,
            config: athena_config,
            buffer_name: buffer_name,
        }
    }

    pub async fn sync(&self) {
        let mut partition_cache: Vec<String> = vec![];

        while let Some(filename) = BufferChunker::next_file(&self.buffer_name) {
            let mut file = BufReader::new(match File::open(&filename) {
                Ok(file) => file,
                Err(err) => {
                    println!(
                        "Failed to open file {} for reading, Error: {}",
                        filename,
                        err.to_string()
                    );
                    continue;
                }
            });

            let mut contents = Vec::new();
            match file.read_to_end(&mut contents) {
                Ok(_bytes) => {}
                Err(err) => {
                    println!(
                        "Failed to read file {}, Error: {}",
                        filename,
                        err.to_string()
                    );
                    continue;
                }
            }

            let _bucket = &self.config.s3_bucket;
            let key = &self.config.s3_prefix;

            let namespace = BufferChunker::decode_file_namespace(&filename);
            // let _time_partition = BufferChunker::decode_file_time(&filename);

            let trimmed_key = &key.trim_matches('/').to_string();

            let mut tags: HashMap<String, String> = HashMap::new();

            let mut full_key = "".to_string();
            if !namespace.is_empty() {
                if !trimmed_key.is_empty() {
                    full_key = format!("{}/{}", trimmed_key, namespace);
                } else {
                    full_key = format!("{}", namespace);
                }

                tags.insert("namespace".to_string(), namespace.to_string());
            }

            // Partitioning
            let mut partition_values: Vec<String> = vec![];

            let partition_path = BufferChunker::decode_file_partition(&filename);

            if !partition_path.is_empty() {
                let parts = partition_path.split('/');
                // let parts = partition_path.split("%2F"); // '/'
                let collection: Vec<&str> = parts.collect();

                for item in &collection {
                    let mut value = match item.split("=").last() {
                        Some(value) => value,
                        None => "",
                    };

                    if value == "" {
                        value = "none";
                    }

                    partition_values.push(value.to_string());
                    tags.insert(item.to_string(), value.to_string());
                }

                // .collect().join("/")
                full_key = format!("{}/{}", full_key, partition_path);
            }

            let time_partition_str = BufferChunker::decode_file_time_to_datetime_string(&filename);

            if !time_partition_str.is_empty() {
                let granularity_target = Config::get_transform_batch_time_unit();

                let date = match DateTime::parse_from_rfc3339(&time_partition_str) {
                    Ok(date) => date,
                    Err(err) => {
                        println!(
                            "Failed to parse time partition string {}, Error: {}",
                            time_partition_str,
                            err.to_string()
                        );
                        continue;
                    }
                };

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
                    partition_values.push(format!("{}", foo));

                    if granularity == &granularity_target {
                        break;
                    }
                }
            }

            if !partition_values.is_empty() {
                let flatten =
                    Config::get_transform_flatten_events();

                let mut out_meta: HashMap<String, Metadata> = HashMap::new();

                // for (namespace, _schema) in &metadata {
                out_meta.insert(namespace.to_string(), Metadata::new().unwrap());

                let metadata = METADATA.read();

                let partition_metadata = if flatten {
                    flatten_metadata(
                        metadata.get(&namespace).unwrap(),
                        &mut out_meta.get_mut(&namespace).unwrap().fields,
                    );
                    out_meta.get(&namespace)
                } else {
                    metadata.get(&namespace)
                };

                if partition_metadata.is_some() {
                    if let Err(_err) = AwsAthena::glue_create_partition(
                        &namespace,
                        partition_values.clone(),
                        &full_key,
                        &mut partition_cache,
                        partition_metadata.unwrap(),
                    )
                    .await
                    {
                        // Handle the error
                    }
                }
                // }
            }

            // md5 hash of the filename
            let md5_digest = md5::compute(&filename);
            let md5_string = hex::encode(&md5_digest.0);

            let final_key = format!("{}/{}", full_key, md5_string);

            DataOutputAwsAthenaPlugin::upload_object(
                &self.s3_client,
                &self.config.s3_bucket,
                &final_key,
                &filename,
                tags,
            )
            .await
            .unwrap();
        }
    }

    // Upload a file to a bucket.
    // snippet-start:[s3.rust.s3-helloworld]
    async fn upload_object(
        client: &S3Client,
        bucket: &str,
        key: &str,
        filename: &str,
        tag_hashmap: HashMap<String, String>
    ) -> Result<(), Error> {
        // let resp = client.list_buckets().send().await?;

        // for bucket in resp.buckets().unwrap_or_default() {
        //     println!("bucket: {:?}", bucket.name().unwrap_or_default())
        // }

        // println!();

        let body = ByteStream::from_path(Path::new(filename)).await;

        let tags = tag_hashmap.iter().map(|(k, v)| format!("{}={}", k, v)).collect::<Vec<String>>().join("&");

        match body {
            Ok(b) => {
                // println!("Uploading file: {} to Bucket: {} and Prefix: {} and Tags: {}", filename, bucket, key, tags);

                match client
                    .put_object()
                    .bucket(bucket)
                    .key(key)
                    .body(b)
                    .tagging(tags)
                    .send()
                    .await
                {
                    Ok(_resp) => {
                        println!("Uploaded {} to S3: {}", filename, key);
                        match fs::remove_file(Path::new(&filename)) {
                            Ok(_) => {}
                            Err(_) => {
                                // @todo - log this back to skippr platform
                            }
                        };
                    }
                    Err(err) => {
                        println!("Failed to upload file: {}, will retry later.", filename);
                        println!("{}", err);

                        // tokio::spawn(async move {
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
                        // });
                        // LOGGER.lock().unwrap().push(format!(
                        //     "Athena Plugin failed to upload file: {}, key: {} with error: {:?}",
                        //     filename,
                        //     key,
                        //     err.into_service_error()
                        // ));
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
            }
        }

        Ok(())
    }
}

pub struct AwsAthena {}

impl AwsAthena {
    pub async fn create_or_update_schema(namespace: &str, schema: &discover::Metadata) {
        match AwsAthena::get_work_group().await {
            Ok(true) => {}
            Ok(false) => {}
            Err(_err) => match AwsAthena::create_workgroup(namespace).await {
                Ok(_) => {}
                Err(err) => {
                    println!("ERROR creating Athena Workgroup: {}", err);
                }
            },
        }

        match AwsAthena::glue_get_database().await {
            Ok(true) => {}
            Ok(false) => {}
            Err(_err) => match AwsAthena::glue_create_database(namespace).await {
                Ok(_) => {}
                Err(err) => {
                    println!("ERROR creating Glue database: {}", err);
                }
            },
        }

        match AwsAthena::glue_get_table(namespace).await {
            Ok(true) => match AwsAthena::glue_update_table(namespace, schema).await {
                Ok(_) => {
                    // println!("Update table {}", namespace)
                }
                Err(err) => {
                    println!("ERROR updating Glue table: {}", err);
                }
            },
            Ok(false) => {}
            Err(_err) => {
                match AwsAthena::glue_create_table(namespace, schema).await {
                    Ok(_) => {}
                    Err(err) => {
                        println!("ERROR creating glue table: {}", err);
                    }
                }
                // println!("Create Hive Table Error: {}", err.into_service_error().to_string())
            }
        }
    }

    pub async fn get_work_group() -> Result<bool, String> {
        let config: DataOutputAwsAthenaPluginConfig = Config::get_pipline_plugin_config("output").unwrap().into();
        let workgroup = config.athena_workgroup_name;
        let aws_config = aws_config::from_env().load().await;

        let athena_client = AthenaClient::new(&aws_config);

        match athena_client
            .get_work_group()
            .work_group(&workgroup)
            .send()
            .await
        {
            Ok(result) => match result.work_group {
                Some(work_group) => {
                    // println!("Found workgroup: {}", workgroup);
                    Ok(work_group.name.unwrap_or_default() == workgroup)
                }
                None => {
                    // println!("Did not find workgroup: {}", workgroup);
                    Ok(false)
                }
            },
            Err(_e) => {
                // println!("Error getting workgroup: {}", &_e.into_service_error().to_string());
                Err(_e.into_service_error().to_string())
            }
        }
    }

    pub async fn glue_get_database() -> Result<bool, String> {
        let config: DataOutputAwsAthenaPluginConfig = Config::get_pipline_plugin_config("output").unwrap().into();

        let database_name = config.glue_database_name;

        let aws_config = aws_config::from_env().load().await;

        let glue_client = GlueClient::new(&aws_config);

        match glue_client.get_database().name(&database_name).send().await {
            Ok(output) => {
                if let Some(database) = output.database {
                    Ok(database.name.unwrap() == database_name)
                } else {
                    Ok(false)
                }
            }
            Err(err) => Err(err.into_service_error().to_string()),
        }
    }

    pub async fn glue_get_table(namespace: &str) -> Result<bool, String> {
        let config: DataOutputAwsAthenaPluginConfig = Config::get_pipline_plugin_config("output").unwrap().into();

        let database_name = config.glue_database_name;

        let aws_config = aws_config::from_env().load().await;

        let glue_client = GlueClient::new(&aws_config);

        match glue_client
            .get_table()
            .database_name(&database_name)
            .name(namespace)
            .send()
            .await
        {
            Ok(output) => {
                if let Some(table) = output.table() {
                    return Ok(table.name().unwrap() == namespace);
                } else {
                    Ok(false)
                }
            }
            Err(err) => Err(err.into_service_error().to_string()),
        }
    }

    pub async fn create_workgroup(_namespace: &str) -> Result<bool, String> {
        let config: DataOutputAwsAthenaPluginConfig = Config::get_pipline_plugin_config("output").unwrap().into();

        let workgroup = config.athena_workgroup_name;
        let bucket = config.s3_bucket;
        let path = config.s3_prefix;
        let path = path.trim_matches('/');

        let path = std::path::Path::new(&bucket)
            .join(&path)
            .join("query-results")
            .to_str()
            .unwrap()
            .to_string();

        let aws_config = aws_config::from_env().load().await;

        let glue_client = AthenaClient::new(&aws_config);

        match glue_client
            .create_work_group()
            .name(&workgroup)
            .description(&workgroup)
            .tags(Tag::builder().key("Name").value(&workgroup).build())
            .tags(Tag::builder().key("Vendor").value("Skippr.io").build())
            .configuration(
                WorkGroupConfiguration::builder()
                    .bytes_scanned_cutoff_per_query(300000000) // 300MB // min is 10000000
                    .enforce_work_group_configuration(true)
                    .publish_cloud_watch_metrics_enabled(false)
                    .requester_pays_enabled(false)
                    .result_configuration(
                        ResultConfiguration::builder()
                            .encryption_configuration(
                                EncryptionConfiguration::builder()
                                    .encryption_option(EncryptionOption::SseS3)
                                    .build(),
                            )
                            .output_location(format!("s3://{}", path))
                            .build(),
                    )
                    .build(),
            )
            .send()
            .await
        {
            Ok(_output) => Ok(true),
            Err(err) => Err(err.into_service_error().to_string()),
        }
    }

    pub async fn glue_create_database(_namespace: &str) -> Result<bool, String> {
        let config: DataOutputAwsAthenaPluginConfig = Config::get_pipline_plugin_config("output").unwrap().into();

        let database = config.glue_database_name;
        let bucket = config.s3_bucket;
        let path = config.s3_prefix;
        let path = path.trim_matches('/');

        let path = std::path::Path::new(&bucket)
            .join(&path)
            .to_str()
            .unwrap()
            .to_string();

        let aws_config = aws_config::from_env().load().await;

        let glue_client = GlueClient::new(&aws_config);

        match glue_client
            .create_database()
            .database_input(
                DatabaseInput::builder()
                    .name(&database)
                    .description(format!("{} managed by skippr.io", database))
                    .location_uri(format!("s3://{}", path))
                    .build(),
            )
            .send()
            .await
        {
            Ok(_output) => Ok(true),
            Err(err) => Err(err.into_service_error().to_string()),
        }
    }

    fn get_partition_by_fields(partitions: &mut Vec<Column>) {

        let partition_config = Config::get_transform_batch_partition_fields();

        if !partition_config.is_empty() {
            let partition_fields: Vec<&str> = partition_config.split(',').collect();

            for field_dot in partition_fields.clone() {
                let entity_name = match field_dot.rfind('.') {
                    Some(index) => format!("p_{}", &field_dot[index + 1..]),
                    None => format!("p_{}", field_dot),
                };
                let clean_field_name = Helpers::clean_field_name(entity_name.to_string());

                partitions.push(
                    Column::builder()
                        .name(clean_field_name.to_string())
                        .r#type("string")
                        .build(),
                );
            }
        }
    }

    pub async fn glue_create_table(
        namespace: &str,
        metadata: &discover::Metadata,
    ) -> Result<bool, String> {
        let config: DataOutputAwsAthenaPluginConfig = Config::get_pipline_plugin_config("output").unwrap().into();

        let database = config.glue_database_name;
        let bucket = config.s3_bucket;
        let granularity_target = Config::get_transform_batch_time_unit();

        let path = config.s3_prefix;
        let path = path.trim_matches('/');
        let path = std::path::Path::new(&bucket)
            .join(&path)
            .join(&namespace)
            .to_str()
            .unwrap()
            .to_string();

        let mut partitions: Vec<Column> = Vec::new();
        let mut partition_indexes: Vec<PartitionIndex> = Vec::new();
        let mut partition_index_keys: Vec<String> = Vec::new();

        AwsAthena::get_partition_by_fields(&mut partitions);

        // Time Partitioning
        if !granularity_target.is_empty() {
            for granularity in GRANULARITIES.iter() {
                partitions.push(
                    Column::builder()
                        .name(granularity.to_string())
                        .r#type("int")
                        .build(),
                );

                if partition_index_keys.len() < 3 {
                    partition_index_keys.push(granularity.to_string());

                    partition_indexes.push(
                        PartitionIndex::builder()
                            .index_name(granularity.to_string())
                            .set_keys(Some(partition_index_keys.clone()))
                            .build(),
                    );
                }

                if granularity == &granularity_target {
                    break;
                }
            }
        }

        let columns = SkipprHive::convert_skippr_to_hive(metadata).unwrap();

        let aws_config = aws_config::from_env().load().await;

        let glue_client = GlueClient::new(&aws_config);

        let mut table_input = TableInput::builder()
            .name(namespace)
            .retention(0)
            .parameters("parquet.compression", "SNAPPY")
            .storage_descriptor(
                StorageDescriptor::builder()
                    .set_columns(Some(columns)) // @todo
                    .compressed(true)
                    .location(format!("s3://{}", path))
                    .input_format("org.apache.hadoop.hive.ql.io.parquet.MapredParquetInputFormat")
                    .output_format("org.apache.hadoop.hive.ql.io.parquet.MapredParquetOutputFormat")
                    .serde_info(
                        SerDeInfo::builder()
                            .name(format!("{}.{}", &database, namespace))
                            .parameters("serialization.format", "1")
                            .serialization_library(
                                "org.apache.hadoop.hive.ql.io.parquet.serde.ParquetHiveSerDe",
                            )
                            .build(),
                    )
                    .stored_as_sub_directories(true)
                    .build(),
            )
            .table_type("EXTERNAL_TABLE");

        if !partitions.is_empty() {
            table_input = table_input.set_partition_keys(Some(partitions));
        }

        let mut create_table_cmd = glue_client
            .create_table()
            .database_name(&database)
            .table_input(table_input.build());

        if !partition_indexes.is_empty() {
            create_table_cmd = create_table_cmd.set_partition_indexes(Some(partition_indexes));
        }

        match create_table_cmd.send().await {
            Ok(_output) => Ok(true),
            Err(err) => {
                println!("{:?}", err);
                Err(err.into_service_error().to_string())
            }
        }
    }

    pub async fn glue_update_table(
        namespace: &str,
        metadata: &discover::Metadata,
    ) -> Result<bool, String> {
        let config: DataOutputAwsAthenaPluginConfig = Config::get_pipline_plugin_config("output").unwrap().into();

        let database = config.glue_database_name;
        let bucket = config.s3_bucket;
        let granularity_target = Config::get_transform_batch_time_unit();

        let path = config.s3_prefix;
        let path = path.trim_matches('/');
        let path = std::path::Path::new(&bucket)
            .join(&path)
            .join(&namespace)
            .to_str()
            .unwrap()
            .to_string();

        let mut partitions: Vec<Column> = Vec::new();
        // let mut partition_indexes: Vec<PartitionIndex> = Vec::new();
        // let mut partition_index_keys:  Vec<String> = Vec::new();

        AwsAthena::get_partition_by_fields(&mut partitions);

        // Time Partitioning
        if !granularity_target.is_empty() {
            for granularity in GRANULARITIES.iter() {
                partitions.push(
                    Column::builder()
                        .name(granularity.to_string())
                        .r#type("int")
                        .build(),
                );

                // partition_index_keys.push(granularity.to_string());

                // partition_indexes.push(
                //     PartitionIndex::builder()
                //         .index_name(granularity_target.to_string())
                //         .set_keys(Some(partition_index_keys.clone()))
                //         .build()
                // );

                if granularity == &granularity_target {
                    break;
                }
            }
        }

        let columns = SkipprHive::convert_skippr_to_hive(metadata).unwrap();

        let aws_config = aws_config::from_env().load().await;

        let glue_client = GlueClient::new(&aws_config);

        let mut table_input = TableInput::builder()
            .name(namespace)
            .retention(0)
            .parameters("parquet.compression", "SNAPPY")
            .storage_descriptor(
                StorageDescriptor::builder()
                    .set_columns(Some(columns)) // @todo
                    .compressed(true)
                    .location(format!("s3://{}", path))
                    .input_format("org.apache.hadoop.hive.ql.io.parquet.MapredParquetInputFormat")
                    .output_format("org.apache.hadoop.hive.ql.io.parquet.MapredParquetOutputFormat")
                    .serde_info(
                        SerDeInfo::builder()
                            .name(format!("{}.{}", &database, namespace))
                            .parameters("serialization.format", "1")
                            .serialization_library(
                                "org.apache.hadoop.hive.ql.io.parquet.serde.ParquetHiveSerDe",
                            )
                            .build(),
                    )
                    .stored_as_sub_directories(true)
                    .build(),
            )
            .table_type("EXTERNAL_TABLE");

        if !partitions.is_empty() {
            // println!("Partition keys: {:?}", partitions);
            table_input = table_input.set_partition_keys(Some(partitions));
        }

        match glue_client
            .update_table()
            .database_name(&database)
            .skip_archive(true)
            .table_input(table_input.build())
            .send()
            .await
        {
            Ok(_output) => Ok(true),
            Err(err) => Err(err.into_service_error().to_string()),
        }
    }

    pub async fn glue_create_partition(
        namespace: &str,
        partition_values: Vec<String>,
        key: &str,
        partition_cache: &mut Vec<String>,
        metadata: &discover::Metadata,
    ) -> Result<bool, Error> {
        let config: DataOutputAwsAthenaPluginConfig = Config::get_pipline_plugin_config("output").unwrap().into();

        let database = config.glue_database_name;
        let bucket = config.s3_bucket;

        // let path = Config::getenv("DATA_OUTPUT_S3_PREFIX", "");
        // let path = path.trim_matches('/');
        // let path = format!("{}/{}", path, key);

        let md5_digest = md5::compute(
            serde_json::to_string(&format!(
                "{}{}{}{:?}",
                namespace,
                bucket,
                key,
                &partition_values.clone()
            ))
            .unwrap(),
        );
        // Convert the digest to a string
        let md5_string = format!("{:x}", md5_digest);
        // Replace "err" as it causes our e2e to fail since they check for 'err' string in logs - yes this happens often enough
        let md5_digest = md5_string.replace("err", "");

        let path = std::path::Path::new(&bucket)
            .join(&key)
            .to_str()
            .unwrap()
            .to_string();

        let columns = SkipprHive::convert_skippr_to_hive(metadata).unwrap();

        let aws_config = aws_config::from_env().load().await;

        let glue_client = GlueClient::new(&aws_config);

        let partition_conf = PartitionInput::builder()
            .set_values(Some(partition_values.clone()))
            .parameters("parquet.compression", "SNAPPY")
            .storage_descriptor(
                StorageDescriptor::builder()
                    .set_columns(Some(columns)) // @todo
                    .compressed(true)
                    .input_format("org.apache.hadoop.hive.ql.io.parquet.MapredParquetInputFormat")
                    .location(format!("s3://{}", path))
                    .output_format("org.apache.hadoop.hive.ql.io.parquet.MapredParquetOutputFormat")
                    .serde_info(
                        SerDeInfo::builder()
                            .name(format!("{}.{}", &database, namespace))
                            .parameters("serialization.format", "1")
                            .serialization_library(
                                "org.apache.hadoop.hive.ql.io.parquet.serde.ParquetHiveSerDe",
                            )
                            .build(),
                    )
                    .stored_as_sub_directories(true)
                    .build(),
            )
            .build();

        if !partition_cache.contains(&md5_digest) {
            match glue_client
                .get_partition()
                .database_name(&database)
                .table_name(namespace)
                .set_partition_values(Some(partition_values.clone()))
                .send()
                .await
            {
                Ok(_) => {
                    // partition exists, update it
                    match glue_client
                        .update_partition()
                        .database_name(database)
                        .table_name(namespace)
                        .partition_input(partition_conf)
                        .set_partition_value_list(Some(partition_values))
                        .send()
                        .await
                    {
                        Ok(_) => {
                            println!("Updated Athena partition");
                            partition_cache.push(md5_digest);
                        }
                        Err(err) => {
                            println!(
                                "Failed to update Athena partition: {}",
                                err.into_service_error()
                            );
                        }
                    }
                }
                Err(_err) => {

                    // partition does not exist, create it
                    match glue_client
                        .create_partition()
                        .database_name(&database)
                        .table_name(namespace)
                        .partition_input(partition_conf)
                        .send()
                        .await
                    {
                        Ok(_) => {
                            println!("Created new Athena partition");
                        }
                        Err(err) => {
                            println!(
                                "Failed to create new Athena partition: {}",
                                err.into_service_error()
                            );
                            println!(
                                "Database: {}, Table: {}, Values: {:?}",
                                database, namespace, partition_values
                            );
                        }
                    }
                } // ,
                  // Err(err) => {
                  //     println!("Failed to get Athena partition: {}", err);
                  // }
            }
            // } else {
            //     println!("Partition already exists in cache");
            //     println!("Database: {}, Table: {}, Values: {:?}", database, namespace, partition_values);
        }

        Ok(true)
    }
}
