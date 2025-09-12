use crate::buffer::BufferChunker;
use crate::converters::skippr_hive::SkipprHive;
use crate::discover::{OutputMetadata, PipelineMetadata};
use crate::helpers::configuration::{Config, PluginConfig};
use crate::helpers::Helpers;
use crate::{METADATA};
use crate::metrics::counters as metrics_counters;
use aws_sdk_athena::types::{EncryptionConfiguration, EncryptionOption, ResultConfiguration, ResultConfigurationUpdates, Tag, WorkGroupConfiguration, WorkGroupConfigurationUpdates};
use aws_sdk_athena::Client as AthenaClient;
use aws_sdk_glue::types::{Column, DatabaseInput, PartitionIndex, PartitionInput, SerDeInfo, StorageDescriptor, TableInput};
use aws_sdk_glue::Client as GlueClient;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::{Client as S3Client, Error};
use aws_sdk_s3::types::{Delete, ObjectIdentifier};

use std::collections::HashMap;
use std::io;
use async_trait::async_trait;
use aws_sdk_glue::error::SdkError;
use aws_sdk_glue::operation::get_table::{GetTableError, GetTableOutput};
use bytes::Bytes;
use datafusion::physical_plan::SendableRecordBatchStream;
use futures::{StreamExt};
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;

use serde_derive::Deserialize;
use crate::ingest::partition_time::TimePartitioner;
use crate::plugins::DataOutputPlugin;
use tokio::sync::{Semaphore, Mutex};
use std::sync::Arc;
use once_cell::sync::Lazy;
use tokio::time::{sleep as tokio_sleep, Duration as TokioDuration};
use dashmap::DashMap;
use rand::Rng;

// Global control-plane throttling and serialization
static GLUE_MAX_CONCURRENCY: Lazy<usize> = Lazy::new(|| {
    Config::getenv("GLUE_MAX_CONCURRENCY", "2").parse::<usize>().unwrap_or(2)
});
static GLUE_CP_SEM: Lazy<Semaphore> = Lazy::new(|| Semaphore::new(*GLUE_MAX_CONCURRENCY));
static ATHENA_WG_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));
static NAMESPACE_LOCKS: Lazy<DashMap<String, Arc<Mutex<()>>>> = Lazy::new(|| DashMap::new());

fn get_namespace_lock(namespace: &str) -> Arc<Mutex<()>> {
    NAMESPACE_LOCKS
        .entry(namespace.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

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
    pub athena_results_s3_bucket: String,

}

pub struct ParquetBytes {
    pub bytes: bytes::Bytes,
    pub size_bytes: u64,
    pub meta_data: parquet::format::FileMetaData,
}

impl From<PluginConfig> for DataOutputAwsAthenaPluginConfig {
    fn from(plugin_config: PluginConfig) -> Self {
        match plugin_config {
            PluginConfig::Athena(athena_config) => athena_config,
            _ => panic!("Invalid plugin type"),
        }
    }
}

#[async_trait]
impl DataOutputPlugin for DataOutputAwsAthenaPlugin {
    async fn sync(&self, stream: SendableRecordBatchStream, filename: String) -> Result<(), std::io::Error> {
    // async fn sync(&mut self, stream: SendableRecordBatchStream, filename: String) -> Result<(), std::io::Error> {
        self.inner_sync(stream, filename).await
    }
}

pub struct DataOutputAwsAthenaPlugin {
    s3_client: S3Client,
    #[allow(dead_code)]
    athena_client: AthenaClient,
    #[allow(dead_code)]
    buffer_name: String,
    config: DataOutputAwsAthenaPluginConfig,
    // s3_bucket: String,
    // s3_prefix: String,
    // time_bucket: String,
    #[allow(dead_code)]
    max_async_uploads: i64,
    upload_sem: Arc<Semaphore>,
}


impl DataOutputAwsAthenaPlugin {

    pub fn get_config() -> DataOutputAwsAthenaPluginConfig {
        match Config::get_pipline_plugin_config("output") {
            Ok(config) => config.into(),
            Err(_) => DataOutputAwsAthenaPluginConfig {
                format: None,
                s3_bucket: Config::getenv("DATA_OUTPUT_S3_BUCKET", ""),
                s3_prefix: Config::getenv("DATA_OUTPUT_S3_PREFIX", ""),
                athena_workgroup_name: Config::getenv("DATA_OUTPUT_ATHENA_WORKGROUP_NAME", ""),
                glue_database_name: Config::getenv("SCHEMA_OUTPUT_GLUE_DATABASE_NAME", ""),
                athena_results_s3_bucket: Config::getenv("DATA_OUTPUT_ATHENA_RESULTS_S3_BUCKET", ""),
            }
        }
    }

    pub async fn new(buffer_name: String) -> DataOutputAwsAthenaPlugin {
        let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest()).load().await;

        let s3_client = S3Client::new(&aws_config);
        let athena_client = AthenaClient::new(&aws_config);

        // let s3_bucket = Config::getenv("DATA_OUTPUT_S3_BUCKET", "");
        // let s3_prefix = Config::getenv("DATA_OUTPUT_S3_PREFIX", "");
        // let time_bucket = Config::getenv("TRANSFORM_BATCH_TIME_UNIT", "");

        // let athena_config: DataOutputAwsAthenaPluginConfig = Config::get_pipline_plugin_config("output").unwrap().into();
        let athena_config: DataOutputAwsAthenaPluginConfig = DataOutputAwsAthenaPlugin::get_config();

        let max_async_uploads_env = Config::getenv("DATA_OUTPUT_MAX_ASYNC_UPLOADS", "16");
        let env_uploads = max_async_uploads_env.parse::<usize>().ok();
        let tuned_uploads = crate::metrics::counters::UPLOAD_CONCURRENCY_TARGET.load(std::sync::atomic::Ordering::Relaxed);
        let max_async_uploads = env_uploads.unwrap_or(tuned_uploads);

        Self {
            s3_client,
            athena_client,
            config: athena_config,
            buffer_name: buffer_name,
            max_async_uploads: max_async_uploads as i64,
            upload_sem: Arc::new(Semaphore::new(max_async_uploads)),
        }
    }

    pub async fn inner_sync(&self, stream: SendableRecordBatchStream, filename: String) -> Result<(), std::io::Error> {
        let mut partition_cache: Vec<String> = vec![];

        let _bucket = &self.config.s3_bucket;
        let key = &self.config.s3_prefix;

        let namespace = BufferChunker::decode_file_namespace(&filename);

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
            let collection: Vec<&str> = parts.collect();

            for item in &collection {
                let mut key = item.split("=").next().unwrap_or_else(|| "");
                let mut value = item.split("=").last().unwrap_or_else(|| "");

                if key == "" {
                    key = "none";
                }

                if value == "" {
                    value = "none";
                }

                partition_values.push(value.to_string());
                tags.insert(key.to_string(), value.to_string());
            }

            full_key = format!("{}/{}", full_key, partition_path);
        }

        let time_partition_str = BufferChunker::decode_file_time_to_datetime_string(&filename);

        if !time_partition_str.is_empty() {
            let time_partition_values = TimePartitioner::new(&filename).get_granularity_values().unwrap();
            let granularity_names = TimePartitioner::get_granularity_names();

            partition_values.extend(time_partition_values.iter().map(|v| v.to_string()));

            granularity_names.iter().enumerate().for_each(|(i, granularity)| {
                full_key = format!("{}/{}={}", full_key, granularity, time_partition_values[i]);
            });
        }

        if !partition_values.is_empty() {
            let flatten = Config::get_transform_flatten_events();
            
            let metadata: PipelineMetadata = METADATA.load().as_ref().clone();
       
            let partition_metadata = if flatten {
                OutputMetadata::from_flatterened_metadata(metadata.metadata.get(&namespace).unwrap())
            } else {
                OutputMetadata::from_metadata(metadata.metadata.get(&namespace).unwrap())
            };

            // Handle partition creation asynchronously
            if let Err(_err) = AwsAthena::glue_create_partition(
                &namespace,
                partition_values.clone(),
                &full_key,
                &mut partition_cache,
                &partition_metadata,
            ).await {
                // Log error but continue with upload
                println!("Warning: Failed to create partition: {}", _err);
            }
        }

        // Generate consistent md5 hash of the filename
        let md5_digest = md5::compute(&filename);
        let md5_string = hex::encode(&md5_digest.0);

        let final_key = format!("{}/{}", full_key, md5_string);

        // Serialize first, then gate S3 upload by a concurrency semaphore
        let parquet = Self::serialize_to_parquet(stream).await?;
        let tags_str = tags.iter().map(|(k, v)| format!("{}={}", k, v)).collect::<Vec<String>>().join("&");

        // Resize semaphore if target changed dynamically
        {
            let target = crate::metrics::counters::UPLOAD_CONCURRENCY_TARGET.load(std::sync::atomic::Ordering::Relaxed);
            let current = self.upload_sem.available_permits() + 1; // approx
            if target as usize != current {
                // Best-effort: add or drain permits to approximate target
                if target as usize > current { self.upload_sem.add_permits(target as usize - current); }
                println!("tune: upload_sem target={} available={} (approx)", target, self.upload_sem.available_permits());
            }
        }
        let _permit = self.upload_sem.clone().acquire_owned().await.map_err(|_| io::Error::new(io::ErrorKind::Other, "Semaphore closed"))?;
        let upload_start = std::time::Instant::now();
        crate::metrics::counters::inc_uploads_in_flight();

        // Perform the S3 upload asynchronously
        let body = ByteStream::from(parquet.bytes.clone());
        match self
            .s3_client
            .put_object()
            .bucket(&self.config.s3_bucket)
            .key(&final_key)
            .body(body)
            .tagging(tags_str)
            .send()
            .await
        {
            Ok(_resp) => {
                println!("Uploaded {} to S3", final_key);
                metrics_counters::add_parquet_bytes(parquet.size_bytes);
                metrics_counters::add_parquet_objects(1);
                metrics_counters::add_parquet_rows(parquet.meta_data.num_rows as u64);
                crate::metrics::counters::add_upload(1);
                crate::metrics::counters::add_upload_latency_ns(upload_start.elapsed().as_nanos() as u64);
                crate::metrics::counters::dec_uploads_in_flight();
                Ok(())
            }
            Err(err) => {
                crate::metrics::counters::dec_uploads_in_flight();
                Err(io::Error::new(io::ErrorKind::Other, format!("Failed to upload to bucket {}, will retry later. Error: {}", self.config.s3_bucket, err.into_service_error())))
            }
        }
    }

    pub(crate) async fn serialize_to_parquet(
        mut batches: SendableRecordBatchStream,
    ) -> Result<ParquetBytes, io::Error> {
        // Get schema from the first batch
        let schema = batches.schema();

        let mut bytes = Vec::new();

        // Configure parquet writer properties
        let props = WriterProperties::builder()
            .set_dictionary_enabled(false)
            .set_encoding(parquet::basic::Encoding::PLAIN)
            .set_compression(Compression::SNAPPY)
            .build();

        let mut writer = ArrowWriter::try_new(
            &mut bytes,
            schema,
            Some(props),
        )?;

        // Process batches asynchronously
        while let Some(batch) = batches.next().await {
            let batch = batch?;
            writer.write(&batch)?;
        }

        // Close writer and get metadata
        let writer_meta = writer.close()?;
        if writer_meta.num_rows == 0 {
            return Err(io::Error::new(io::ErrorKind::Other, "No rows to write to parquet"));
        }

        let size_bytes = bytes.len() as u64;

        Ok(ParquetBytes {
            meta_data: writer_meta,
            bytes: Bytes::from(bytes),
            size_bytes,
        })
    }

    async fn upload_object(
        client: S3Client,
        bucket: String,
        key: String,
        stream: SendableRecordBatchStream,
        tag_hashmap: HashMap<String, String>
    ) -> Result<(), std::io::Error> {

        let tags = tag_hashmap.iter().map(|(k, v)| format!("{}={}", k, v)).collect::<Vec<String>>().join("&");

        // Serialize to parquet asynchronously
        let parquet = Self::serialize_to_parquet(stream).await?;

        // Create the upload body stream
        let body = ByteStream::from(parquet.bytes);

        // NOTE: Unused now; upload is performed in inner_sync with concurrency gating
        unreachable!("upload_object is not used after enabling gated concurrency in inner_sync")
    }
}

pub struct AwsAthena {}

impl AwsAthena {
    // Generic backoff helper for Glue/Athena control-plane
    // Interprets "RETRY_TRANSIENT" as a signal to retry
    async fn backoff_retry<F, Fut, T>(mut op: F, op_name: &str) -> Result<T, String>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, String>>,
    {
        let mut attempt: u32 = 0;
        loop {
            match op().await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    let s = e.to_string();
                    // Missing region/creds: don't spin forever, report once
                    if s.contains("Missing Region") || s.contains("CredentialsNotLoaded") {
                        return Err(s);
                    }
                    // Transient or explicit retry signal
                    if s.contains("Throttling") || s.contains("TooManyRequests") || s.contains("ConcurrentModification") || s == "RETRY_TRANSIENT" {
                        attempt += 1;
                        if attempt > 6 { return Err(s); }
                        let base = 200u64 * (1u64 << attempt.min(6));
                        let jitter: u64 = rand::thread_rng().gen_range(0..100);
                        let delay_ms = base + jitter;
                        println!("Glue/Athena {} retry {} in {}ms: {}", op_name, attempt, delay_ms, s);
                        tokio_sleep(TokioDuration::from_millis(delay_ms)).await;
                        continue;
                    }
                    return Err(s);
                }
            }
        }
    }
    pub async fn create_or_update_schema(namespace: &str, schema: &OutputMetadata) {
        // Serialize workgroup changes to avoid Athena InvalidRequestException on concurrent updates
        let _wg_guard = ATHENA_WG_LOCK.lock().await;
        match AwsAthena::get_work_group().await {
            Ok(true) => {}
            Ok(false) => {}
            Err(_err) => match AwsAthena::create_workgroup().await {
                Ok(_) => {
                    println!("Created Athena Workgroup");
                }
                Err(_err) => {
                    match AwsAthena::update_workgroup().await {
                        Ok(_) => {
                            println!("Updated Athena Workgroup");
                        }
                        Err(err) => {
                            println!("ERROR creating/updating Athena Workgroup: {}", err);
                        }
                    }
                }
            },
        }
        drop(_wg_guard);

        // Limit Glue control-plane concurrency globally
        let _cp_permit = GLUE_CP_SEM.acquire().await.unwrap();

        // Serialize by namespace to avoid ConcurrentModificationException
        let ns_lock = get_namespace_lock(namespace);
        let _ns_guard = ns_lock.lock().await;

        match AwsAthena::glue_get_database().await {
            Ok(true) => {}
            Ok(false) => {}
            Err(_err) => {
                // Create database with backoff; AlreadyExists => success
                if let Err(err) = AwsAthena::backoff_retry(|| AwsAthena::glue_create_database(), "create_database").await {
                    println!("ERROR creating Glue database: {}", err);
                }
            }
        }

        match AwsAthena::glue_get_table(namespace).await {
            Ok(table) => {
                if let Err(err) = AwsAthena::backoff_retry(|| AwsAthena::glue_update_table(namespace, schema, table.clone()), "update_table").await {
                    println!("ERROR updating Glue table: {}", err);
                }
            }
            Err(_err) => {
                // Create table with backoff; AlreadyExists => success
                if let Err(err) = AwsAthena::backoff_retry(|| AwsAthena::glue_create_table(namespace, schema), "create_table").await {
                    println!("ERROR creating glue table: {}", err);
                }
                // println!("Create Hive Table Error: {}", err.into_service_error().to_string())
            }
        }
    }

    pub async fn get_work_group() -> Result<bool, String> {
        let config: DataOutputAwsAthenaPluginConfig = DataOutputAwsAthenaPlugin::get_config();
        let workgroup = config.athena_workgroup_name;
        let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest()).load().await;

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
                    Ok(work_group.name == workgroup)
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
        let config: DataOutputAwsAthenaPluginConfig = DataOutputAwsAthenaPlugin::get_config();

        let database_name = config.glue_database_name;

        let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest()).load().await;

        let glue_client = GlueClient::new(&aws_config);

        match glue_client.get_database().name(&database_name).send().await {
            Ok(output) => {
                if let Some(database) = output.database {
                    Ok(database.name == database_name)
                } else {
                    Ok(false)
                }
            }
            Err(err) => Err(err.into_service_error().to_string()),
        }
    }

    pub async fn glue_get_table(namespace: &str) -> Result<GetTableOutput, SdkError<GetTableError>> {
        let config: DataOutputAwsAthenaPluginConfig = DataOutputAwsAthenaPlugin::get_config();

        let database_name = config.glue_database_name;

        let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest()).load().await;

        let glue_client = GlueClient::new(&aws_config);

        glue_client
            .get_table()
            .database_name(&database_name)
            .name(namespace)
            .send()
            .await
    }

    pub async fn create_workgroup() -> Result<bool, String> {
        let config: DataOutputAwsAthenaPluginConfig = DataOutputAwsAthenaPlugin::get_config();

        let workgroup = config.athena_workgroup_name;
        let bucket = config.athena_results_s3_bucket;
        let path = config.s3_prefix;
        let path = path.trim_matches('/');

        let path = std::path::Path::new(&bucket)
            .join(&path)
            .join("query-results")
            .to_str()
            .unwrap()
            .to_string();

        let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest()).load().await;

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
                                    .build()
                                    .unwrap(),
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

    pub async fn update_workgroup() -> Result<bool, String> {
        let config: DataOutputAwsAthenaPluginConfig = DataOutputAwsAthenaPlugin::get_config();

        let workgroup = config.athena_workgroup_name;
        let bucket = config.athena_results_s3_bucket;
        let path = config.s3_prefix;
        let path = path.trim_matches('/');

        let path = std::path::Path::new(&bucket)
            .join(&path)
            .join("query-results")
            .to_str()
            .unwrap()
            .to_string();

        let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest()).load().await;

        let glue_client = AthenaClient::new(&aws_config);

        match glue_client
            .update_work_group()
            .work_group(&workgroup)
            .configuration_updates(
                WorkGroupConfigurationUpdates::builder()
                    .bytes_scanned_cutoff_per_query(300000000) // 300MB // min is 10000000
                    .enforce_work_group_configuration(true)
                    .publish_cloud_watch_metrics_enabled(false)
                    .requester_pays_enabled(false)
                    .result_configuration_updates(
                        ResultConfigurationUpdates::builder()
                            .encryption_configuration(
                                EncryptionConfiguration::builder()
                                    .encryption_option(EncryptionOption::SseS3)
                                    .build()
                                    .unwrap(),
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

    pub async fn glue_create_database() -> Result<bool, String> {
        let config: DataOutputAwsAthenaPluginConfig = DataOutputAwsAthenaPlugin::get_config();

        let database = config.glue_database_name;
        let bucket = config.s3_bucket;
        let path = config.s3_prefix;
        let path = path.trim_matches('/');

        let path = std::path::Path::new(&bucket)
            .join(&path)
            .to_str()
            .unwrap()
            .to_string();

        let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest()).load().await;

        let glue_client = GlueClient::new(&aws_config);

        match glue_client
            .create_database()
            .database_input(
                DatabaseInput::builder()
                    .name(&database)
                    .description(format!("{} managed by skippr.io", database))
                    .location_uri(format!("s3://{}", path))
                    .build()
                    .unwrap(),
            )
            .send()
            .await
        {
            Ok(_output) => Ok(true),
            Err(err) => {
                let s = err.into_service_error().to_string();
                if s.contains("AlreadyExistsException") {
                    return Ok(true);
                }
                Err(s)
            }
        }
    }

    pub async fn delete_glue_database(database_name: &str) -> Result<bool, String> {

        loop {
            println!("Are you sure you want to drop the database? To confirm, please type the database name ('{}'). Type 'exit' or ctrl+c to cancel:", database_name);

            let mut input = String::new();
            io::stdin().read_line(&mut input).unwrap_or_default();

            if input.trim() == database_name {
                println!("Dropping database '{}'", database_name);

                let mut timeout = 10;

                println!("Waiting {} seconds before dropping database '{}', ctrl+c to cancel", timeout, database_name);

                loop {
                    if timeout > 0 {
                        tokio::time::sleep(tokio::time::Duration::from_secs(timeout)).await;
                        timeout -= 1;
                    }  else {
                        break;
                    }
                }
                
                break;

            } else if input.trim().eq_ignore_ascii_case("exit") {
                // println!("Drop canceled. Exiting without dropping database.");
                return Err(format!("Drop canceled. Exiting without dropping database '{}'.", database_name));
            } else {
                // println!("Incorrect database name. Please try again, or type 'exit' to cancel.");
                return Err(format!("Incorrect database name entered: '{}'.", input.trim()));
                // The loop will continue, prompting the user again
            }
        }

        let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest()).load().await;

        let glue_client = GlueClient::new(&aws_config);

        // cascade delete

        // recursively list tables and their partitions, deleting the partitions in batches of 25 and then the tables in batches of 25
        let output = match glue_client
            .get_tables()
            .database_name(database_name)
            .send()
            .await
        {
            Ok(output) => output,
            Err(err) => return Err(err.into_service_error().to_string()),
        };

        if let Some(tables) = output.table_list {

            if tables.is_empty() {
                println!("No tables found in database '{}'", database_name);
            }

            println!("Deleting tables in database '{}'", database_name);

            for table in tables {
                let table_name = table.name;
                
                println!("Deleting table '{}'", table_name);

                let mut next_token = "".to_string();

                // delete partitions in batches of 25, recursing through pages via next_token
                while let Ok(partitions) = glue_client
                    .get_partitions()
                    .database_name(database_name)
                    .table_name(&table_name)
                    .max_results(25)
                    .next_token(next_token)
                    .send()
                    .await
                {
                    if let Some(partitions) = partitions.partitions {

                        if partitions.is_empty() {
                            break;
                        }

                        println!("Deleting {} partitions in table '{}'", partitions.len(), table_name);

                        for partition in partitions {
                            glue_client
                                .delete_partition()
                                .database_name(database_name)
                                .table_name(&table_name)
                                .set_partition_values(partition.values)
                                .send()
                                .await
                                .unwrap();
                        }
                    }
                    if partitions.next_token.is_none() {
                        break;
                    }
                    next_token = partitions.next_token.unwrap()
                }

                let mut next_token = "".to_string();

                // Delete table versions
                while let Ok(table_versions) = glue_client
                    .get_table_versions()
                    .database_name(database_name)
                    .table_name(&table_name)
                    .max_results(25)
                    .next_token(next_token)
                    .send()
                    .await
                {
                    if let Some(table_versions) = table_versions.table_versions {

                        if table_versions.is_empty() {
                            println!("No table versions found for table '{}'", table_name);
                            break;
                        }

                        println!("Deleting {} table versions", table_versions.len());

                        for version in table_versions {
                            let version_id = version.version_id;
                            match glue_client
                                .delete_table_version()
                                .database_name(database_name)
                                .table_name(&table_name)
                                .set_version_id(version_id)
                                .send()
                                .await
                            {
                                Ok(_output) => {}
                                Err(err) => return Err(err.into_service_error().to_string()),
                            }
                        }
                    }
                    if table_versions.next_token.is_none() {
                        break;
                    }
                    next_token = table_versions.next_token.unwrap()
                }

                // delete the table
                glue_client
                    .delete_table()
                    .database_name(database_name)
                    .name(&table_name)
                    .send()
                    .await
                    .unwrap();

                println!("Deleted table '{}'", table_name);
            }
        }

        // get database s3 bucket and path
        let location_uri= glue_client
            .get_database()
            .name(database_name)
            .send()
            .await
            .unwrap()
            .database
            .unwrap()
            .location_uri
            .unwrap();

        let bucket = location_uri.split('/').nth(2).unwrap();
        let path = location_uri.split('/').skip(3).collect::<Vec<&str>>().join("/");

        match glue_client
            .delete_database()
            .name(database_name)
            .send()
            .await
        {
            Ok(_output) => println!("Deleted database {}", database_name),
            Err(_err) => (),
        }

        // delete contents from the s3 bucket
        let s3_client = S3Client::new(&aws_config);

        // let bucket = config.s3_bucket;
        // let path = config.s3_prefix;
        // let path = path.trim_matches('/');
        // let path = std::path::Path::new(&bucket)
        //     .join(&path)
        //     .to_str()
        //     .unwrap()
        //     .to_string();

        let mut next_token = None;

        println!("Deleting table data objects from {}/{}", bucket, path);

        while let Ok(resp) = s3_client
            .list_objects_v2()
            .bucket(bucket)
            .prefix(&path)
            .max_keys(1000)
            .set_continuation_token(next_token.clone())
            .send()
            .await {
            let mut delete_objects: Vec<ObjectIdentifier> = vec![];

            if resp.contents.is_none() {
                continue
            };

            let objects = resp.contents();

            for obj in objects {
                
                let obj_id = ObjectIdentifier::builder()
                    .set_key(obj.key.clone())
                    .build();
                
                match obj_id {
                    Ok(obj_id) => {
                        delete_objects.push(obj_id);
                    }
                    Err(_) => {
                        // println!("Failed to create object identifier for key: {}", obj.key.unwrap_or_default());
                    }
                }
            }
            

            if !delete_objects.is_empty() {

                println!("Deleting {} S3 objects from bucket {}", delete_objects.len(), bucket);

                s3_client
                    .delete_objects()
                    .bucket(bucket)
                    .delete(
                        Delete::builder()
                            .set_objects(Some(delete_objects))
                            .build()
                            .unwrap(),
                    )
                    .send()
                    .await.unwrap();
            }

            if resp.next_continuation_token.is_none() {
                break;
            }
            next_token = resp.next_continuation_token;
        }

        Ok(true)
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
                        .build()
                        .unwrap(),
                );
            }
        }
    }


    pub async fn glue_delete_table(
        namespace: &str,
    ) -> Result<bool, String> {
        let config: DataOutputAwsAthenaPluginConfig = DataOutputAwsAthenaPlugin::get_config();

        let database = config.glue_database_name;

        let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest()).load().await;

        let glue_client = GlueClient::new(&aws_config);

        match glue_client
            .delete_table()
            .database_name(&database)
            .name(namespace)
            .send()
            .await
        {
            Ok(_output) => Ok(true),
            Err(err) => {
                println!("{:?}", err);
                Err(err.into_service_error().to_string())
            }
        }
    }

    pub async fn glue_create_table(
        namespace: &str,
        metadata: &OutputMetadata,
    ) -> Result<bool, String> {
        let config: DataOutputAwsAthenaPluginConfig = DataOutputAwsAthenaPlugin::get_config();

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
            for granularity in TimePartitioner::get_granularity_names() {
                partitions.push(
                    Column::builder()
                        .name(granularity.to_string())
                        .r#type("int")
                        .build()
                        .unwrap(),
                );

                if partition_index_keys.len() < 3 {
                    partition_index_keys.push(granularity.to_string());

                    partition_indexes.push(
                        PartitionIndex::builder()
                            .index_name(granularity.to_string())
                            .set_keys(Some(partition_index_keys.clone()))
                            .build()
                            .unwrap(),
                    );
                }

                if granularity == granularity_target {
                    break;
                }
            }
        }

        let columns = SkipprHive::convert_skippr_to_hive(metadata).unwrap();

        let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest()).load().await;

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
            .table_input(table_input.build().unwrap());

        if !partition_indexes.is_empty() {
            create_table_cmd = create_table_cmd.set_partition_indexes(Some(partition_indexes));
        }

        match create_table_cmd.send().await {
            Ok(_output) => Ok(true),
            Err(err) => {
                let s = err.into_service_error().to_string();
                if s.contains("AlreadyExistsException") {
                    return Ok(true);
                }
                Err(s)
            }
        }
    }

    pub async fn glue_update_table(
        namespace: &str,
        metadata: &OutputMetadata,
        existing_table: GetTableOutput
    ) -> Result<bool, String> {
        let config: DataOutputAwsAthenaPluginConfig = DataOutputAwsAthenaPlugin::get_config();

        let database = config.glue_database_name;
        let bucket = config.s3_bucket;

        let path = config.s3_prefix;
        let path = path.trim_matches('/');
        let path = std::path::Path::new(&bucket)
            .join(&path)
            .join(&namespace)
            .to_str()
            .unwrap()
            .to_string();

        let columns = SkipprHive::convert_skippr_to_hive(metadata).unwrap();

        let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest()).load().await;

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


        // Not valid to update partitions, would require a migration of all data and partition indexes.
        // Additionally, when issuing `ALTER SCHEMA` - we may be local and not have a config file specifying
        // the partition time unit (year, month, day, hour, minute).
        // So we inherit the existing partition keys.
        if existing_table.table().is_some() {
            let existing_table = existing_table.table().unwrap();
            table_input = table_input.set_partition_keys(existing_table.partition_keys.clone());
        }

        match glue_client
            .update_table()
            .database_name(&database)
            .skip_archive(true)
            .table_input(table_input.build().unwrap())
            .send()
            .await
        {
            Ok(_output) => Ok(true),
            Err(err) => {
                let s = err.into_service_error().to_string();
                // Treat concurrent modification as transient
                if s.contains("ConcurrentModificationException") {
                    return Err("RETRY_TRANSIENT".to_string());
                }
                Err(s)
            }
        }
    }

    pub async fn glue_create_partition(
        namespace: &str,
        partition_values: Vec<String>,
        key: &str,
        partition_cache: &mut Vec<String>,
        metadata: &OutputMetadata,
    ) -> Result<bool, Error> {
        let config: DataOutputAwsAthenaPluginConfig = DataOutputAwsAthenaPlugin::get_config();

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

        let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest()).load().await;

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
            // Gate by global semaphore and per-namespace mutex
            let _cp_permit = GLUE_CP_SEM.acquire().await.unwrap();
            let ns_lock = get_namespace_lock(namespace);
            let _ns_guard = ns_lock.lock().await;

            match glue_client
                .get_partition()
                .database_name(&database)
                .table_name(namespace)
                .set_partition_values(Some(partition_values.clone()))
                .send()
                .await
            {
                Ok(_) => {
                    // partition exists, update it with backoff
                    let update_res: Result<(), String> = AwsAthena::backoff_retry(|| async {
                        glue_client
                            .update_partition()
                            .database_name(database.clone())
                            .table_name(namespace)
                            .partition_input(partition_conf.clone())
                            .set_partition_value_list(Some(partition_values.clone()))
                            .send()
                            .await
                            .map(|_| true)
                            .map_err(|e| e.into_service_error().to_string())
                    }, "update_partition").await.map(|_| ());

                    if let Err(err) = update_res {
                        println!("Failed to update Athena partition: {}", err);
                    } else {
                        partition_cache.push(md5_digest);
                    }
                }
                Err(_err) => {
                    // partition does not exist, create it with backoff; AlreadyExists is fine
                    let create_res: Result<(), String> = AwsAthena::backoff_retry(|| async {
                        match glue_client
                            .create_partition()
                            .database_name(&database)
                            .table_name(namespace)
                            .partition_input(partition_conf.clone())
                            .send()
                            .await {
                                Ok(_) => Ok(true),
                                Err(e) => {
                                    let s = e.into_service_error().to_string();
                                    if s.contains("AlreadyExistsException") { Ok(true) } else { Err(s) }
                                }
                            }
                    }, "create_partition").await.map(|_| ());

                    match create_res {
                        Ok(_) => {
                            println!("Created new Athena partition");
                        }
                        Err(err) => {
                            println!("Failed to create new Athena partition: {}", err);
                            println!("Database: {}, Table: {}, Values: {:?}", database, namespace, partition_values);
                        }
                    }
                }
            }
        }

        Ok(true)
    }
}

