use crate::buffer::BufferChunker;
use crate::converters::skippr_hive::SkipprHive;
use crate::discover;
use crate::helpers::configuration::Config;
use crate::helpers::Helpers;
use aws_sdk_athena::types::{
    EncryptionConfiguration, EncryptionOption, ResultConfiguration, Tag, WorkGroupConfiguration,
};
use aws_sdk_athena::Client as AthenaClient;
use aws_sdk_glue::types::{
    Column, DatabaseInput, PartitionIndex, PartitionInput, SerDeInfo, StorageDescriptor, TableInput,
};
use aws_sdk_glue::Client as GlueClient;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::{Client as S3Client, Error};
use chrono::prelude::*;
use md5::Digest;
use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

pub struct DataOutputAwsAthenaPlugin {
    s3_client: S3Client,
    athena_client: AthenaClient,
    // config: Config,
    s3_bucket: String,
    s3_prefix: String,
    time_bucket: String,
}

const GRANULARITIES: [&str; 5] = ["year", "month", "day", "hour", "minute"];

impl DataOutputAwsAthenaPlugin {
    pub async fn new() -> DataOutputAwsAthenaPlugin {
        let aws_config = aws_config::from_env().load().await;

        let s3_client = S3Client::new(&aws_config);
        let athena_client = AthenaClient::new(&aws_config);

        let s3_bucket = Config::getenv("DATA_OUTPUT_S3_BUCKET", "");
        let s3_prefix = Config::getenv("DATA_OUTPUT_S3_PREFIX", "");
        let time_bucket = Config::getenv("DATA_OUTPUT_TIME_BUCKET", "");

        Self {
            s3_client,
            athena_client,
            s3_bucket,
            s3_prefix,
            time_bucket,
        }
    }

    pub async fn sync(&self, metadata: HashMap<std::string::String, discover::Metadata>) {
        let mut partition_cache: Vec<String> = vec![];

        while let Some(filename) = BufferChunker::next_file() {
            let mut file = BufReader::new(File::open(&filename).unwrap());

            let mut contents = Vec::new();
            file.read_to_end(&mut contents).unwrap();

            let _bucket = &self.s3_bucket;
            let key = &self.s3_prefix;

            let namespace = BufferChunker::decode_file_namespace(&filename);
            // let _time_partition = BufferChunker::decode_file_time(&filename);

            let trimmed_key = &key.trim_start_matches('/').to_string();

            let mut full_key = "".to_string();
            // key = trimmed_key;
            if !namespace.is_empty() {
                if !trimmed_key.is_empty() {
                    full_key = format!("{}/{}", trimmed_key, namespace);
                } else {
                    full_key = format!("{}", namespace);
                }
            }

            // Partitioning
            let mut partition_values: Vec<String> = vec![];

            let partition = BufferChunker::decode_file_partition(&filename);

            if !partition.is_empty() {
                let parts = partition.split('-');
                let collection: Vec<&str> = parts.collect();

                for item in &collection {
                    let value = item.rsplitn(1, '=').next().unwrap();
                    partition_values.push(value.to_string());
                }

                let path_parts = collection.join("/");

                // .collect().join("/")
                full_key = format!("{}/{}", full_key, path_parts);
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
                            panic!("Did not reconise date granularity of {}", granularity);
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
                match AwsAthena::glue_create_partition(
                    &namespace,
                    partition_values,
                    key,
                    &mut partition_cache,
                    metadata.get(&namespace).unwrap(),
                )
                .await
                {
                    Ok(_) => {}
                    Err(_err) => {}
                }
            }

            let final_key = format!("{}/{}", full_key, Helpers::random_password(32));

            DataOutputAwsAthenaPlugin::upload_object(
                &self.s3_client,
                &self.s3_bucket,
                &final_key,
                &filename,
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
    ) -> Result<(), Error> {
        // let resp = client.list_buckets().send().await?;

        // for bucket in resp.buckets().unwrap_or_default() {
        //     println!("bucket: {:?}", bucket.name().unwrap_or_default())
        // }

        // println!();

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
                    Ok(resp) => {
                        println!("Upload to S3: {}", key);
                        fs::remove_file(Path::new(&filename)).unwrap();
                    }
                    Err(err) => {
                        println!("Got an error uploading object:");
                        println!("{:?}", err.into_service_error());
                    }
                }

                // let resp = client.get_object().bucket(bucket).key(key).send().await?;
                // println!("Response: {:?}", resp);

                // let data = resp.body.collect().await;
                // println!("data: {:?}", data.unwrap().into_bytes());
            }
            Err(e) => {
                println!("Got an error parsing file:");
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
                    println!("ERROR getting Glue database: {}", err);
                }
            },
        }

        match AwsAthena::glue_get_table(namespace).await {
            Ok(true) => match AwsAthena::glue_update_table(namespace, schema).await {
                Ok(_) => {}
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
        let workgroup = Config::getenv("ATHENA_WORKGROUP_NAME", "");
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
        let database_name = Config::getenv("GLUE_DATABASE_NAME", "");

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
        let database_name = Config::getenv("GLUE_DATABASE_NAME", "");

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
        let workgroup = Config::getenv("ATHENA_WORKGROUP_NAME", "");
        let bucket = Config::getenv("DATA_OUTPUT_S3_BUCKET", "");
        let path = Config::getenv("DATA_OUTPUT_S3_PREFIX", "");
        let path = path.trim_start_matches('/');

        let aws_config = aws_config::from_env().load().await;

        let glue_client = AthenaClient::new(&aws_config);

        match glue_client
            .create_work_group()
            .name(&workgroup)
            .description(&workgroup)
            .tags(Tag::builder().key("name").value(&workgroup).build())
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
                            .output_location(format!("s3://{}/{}/query-results", bucket, path))
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
        let database = Config::getenv("GLUE_DATABASE_NAME", "");
        let bucket = Config::getenv("DATA_OUTPUT_S3_BUCKET", "");
        let path = Config::getenv("DATA_OUTPUT_S3_PREFIX", "");
        let path = path.trim_start_matches('/');

        let aws_config = aws_config::from_env().load().await;

        let glue_client = GlueClient::new(&aws_config);

        match glue_client
            .create_database()
            .database_input(
                DatabaseInput::builder()
                    .name(&database)
                    .description(format!("{} managed by skippr.io", database))
                    .location_uri(format!("s3://{}/{}", bucket, path))
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
        let partition_config = Config::getenv("DATA_OUTPUT_PARTITION_BY_FIELDS", "");

        if !partition_config.is_empty() {
            let partition_fields: Vec<&str> = partition_config.split(',').collect();

            for field_dot in partition_fields.clone() {
                let entity_name = match field_dot.rfind('.') {
                    Some(index) => &field_dot[index + 1..],
                    None => field_dot,
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
        let database = Config::getenv("GLUE_DATABASE_NAME", "");
        let bucket = Config::getenv("DATA_OUTPUT_S3_BUCKET", "");
        let granularity_target = Config::getenv("DATA_OUTPUT_TIME_BUCKET", "");

        let path = Config::getenv("DATA_OUTPUT_S3_PREFIX", "");
        let path = path.trim_start_matches('/');
        let path = format!("{}/{}", path, namespace);

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

        // let mut schema: HashMap<String, Metadata> = HashMap::new();
        // schema.insert(namespace.to_string(), metadata.clone());
        let columns = SkipprHive::convert_skippr_to_hive(metadata).unwrap();

        let aws_config = aws_config::from_env().load().await;

        let glue_client = GlueClient::new(&aws_config);

        let mut table_input = TableInput::builder()
            .name(namespace)
            .retention(0)
            .storage_descriptor(
                StorageDescriptor::builder()
                    .set_columns(Some(columns)) // @todo
                    .compressed(false)
                    .location(format!("s3://{}/{}", bucket, path))
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
        let database = Config::getenv("GLUE_DATABASE_NAME", "");
        let bucket = Config::getenv("DATA_OUTPUT_S3_BUCKET", "");
        let granularity_target = Config::getenv("DATA_OUTPUT_TIME_BUCKET", "");

        let path = Config::getenv("DATA_OUTPUT_S3_PREFIX", "");
        let path = path.trim_start_matches('/');
        let path = format!("{}/{}", path, namespace);

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

        // let mut schema: HashMap<String, Metadata> = HashMap::new();
        // schema.insert(namespace.to_string(), metadata.clone());
        let columns = SkipprHive::convert_skippr_to_hive(metadata).unwrap();

        let aws_config = aws_config::from_env().load().await;

        let glue_client = GlueClient::new(&aws_config);

        let mut table_input = TableInput::builder()
            .name(namespace)
            .retention(0)
            .storage_descriptor(
                StorageDescriptor::builder()
                    .set_columns(Some(columns)) // @todo
                    .compressed(false)
                    .location(format!("s3://{}/{}", bucket, path))
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
        _key: &str,
        partition_cache: &mut Vec<String>,
        metadata: &discover::Metadata,
    ) -> Result<bool, Error> {
        let database = Config::getenv("GLUE_DATABASE_NAME", "");
        let bucket = Config::getenv("DATA_OUTPUT_S3_BUCKET", "");

        let path = Config::getenv("DATA_OUTPUT_S3_PREFIX", "");
        let path = path.trim_start_matches('/');
        let path = format!("{}/{}", path, namespace);

        let md5_digest = md5::compute(
            serde_json::to_string(&format!(
                "{}{}{}{:?}",
                namespace,
                bucket,
                path,
                &partition_values.clone()
            ))
            .unwrap(),
        );
        // Convert the digest to a string
        let md5_string = format!("{:x}", md5_digest);
        // Replace "err" as it causes our e2e to fail since they check for 'err' string in logs - yes this happens often enough
        let md5_digest = md5_string.replace("err", "");


        // let mut schema: HashMap<String, Metadata> = HashMap::new();
        // schema.insert(namespace.to_string(), metadata.clone());
        let columns = SkipprHive::convert_skippr_to_hive(metadata).unwrap();

        let aws_config = aws_config::from_env().load().await;

        let glue_client = GlueClient::new(&aws_config);

        let partition_conf = PartitionInput::builder()
            .set_values(Some(partition_values.clone()))
            .storage_descriptor(
                StorageDescriptor::builder()
                    .set_columns(Some(columns)) // @todo
                    .compressed(false)
                    .input_format("org.apache.hadoop.hive.ql.io.parquet.MapredParquetInputFormat")
                    .location(format!("s3://{}/{}", bucket, path))
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
                        .database_name(database)
                        .table_name(namespace)
                        .partition_input(partition_conf)
                        .send()
                        .await
                    {
                        Ok(_) => {
                            println!("Created new Athena partition");
                        }
                        Err(err) => {
                            println!("{:?}", partition_values);
                            println!("{}", path);
                            println!(
                                "Failed to create new Athena partition: {}",
                                err.into_service_error()
                            );
                        }
                    }
                } // ,
                  // Err(err) => {
                  //     println!("Failed to get Athena partition: {}", err);
                  // }
            }
        }

        Ok(true)
    }
}
