use crate::helpers::configuration::DataSinkPluginConfig;
use crate::helpers::Helpers;
use aws_sdk_athena::types::{
    EncryptionConfiguration, EncryptionOption, ResultConfiguration, ResultConfigurationUpdates,
    Tag, WorkGroupConfiguration, WorkGroupConfigurationUpdates,
};
use aws_sdk_athena::Client as AthenaClient;
use aws_sdk_glue::types::{
    Column, DatabaseInput, PartitionIndex, PartitionInput, SerDeInfo, StorageDescriptor, TableInput,
};
use aws_sdk_glue::Client as GlueClient;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use aws_sdk_s3::types::{Delete, ObjectIdentifier};
use aws_sdk_s3::Client as S3Client;
use skippr_runtime_sdk::converters::skippr_hive::SkipprHive;
use skippr_runtime_sdk::discover::{OutputMetadata, SkipprDataType};
use skippr_runtime_sdk::metrics::counters as metrics_counters;
use skippr_runtime_sdk::plugins::{SchemaSink, SchemaSyncRequest};
use skippr_runtime_sdk::sink_compat::BufferChunker;

use arrow::array::RecordBatch;
use arrow::util::display::array_value_to_string;
use async_trait::async_trait;
use aws_sdk_glue::error::SdkError;
use aws_sdk_glue::operation::get_table::{GetTableError, GetTableOutput};
use bytes::Bytes;
use datafusion::physical_plan::RecordBatchStream;
use datafusion::physical_plan::SendableRecordBatchStream;
use futures::StreamExt;
use parquet::arrow::ArrowWriter;
use std::collections::{BTreeMap, HashMap};
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll as TaskPoll};
use tokio::task::block_in_place;

use super::parquet_util::serialize_to_parquet;
use dashmap::DashMap;
use once_cell::sync::Lazy;
use rand::Rng;
use serde_derive::Deserialize;
use skippr_runtime_sdk::plugins::source_contract::{
    ensure_source_contract_for_policy, namespace_source_contract, validate_write_policy_for_sink,
    SinkWritePolicySupport, SourceNamespaceContract, WritePolicy,
};
use skippr_runtime_sdk::plugins::{DataSink, SinkWriteContext};
use skippr_runtime_sdk::protocol::{RuntimeBinding, RuntimeExecutionContext};
use skippr_runtime_sdk::sink_compat::partition_time::TimePartitioner;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{Mutex, RwLock, Semaphore};
use tokio::time::{sleep as tokio_sleep, Duration as TokioDuration};
use tracing::{debug, info, warn};

const TIME_PARTITION_GRANULARITIES: [&str; 5] = ["year", "month", "day", "hour", "minute"];

const ATHENA_WRITE_POLICY_SUPPORT: SinkWritePolicySupport = SinkWritePolicySupport {
    supports_merge_by_key: false,
    supports_replace_partition: true,
    supports_replace_table: true,
};

// Global control-plane throttling and serialization
const DEFAULT_GLUE_CONTROL_PLANE_CONCURRENCY: usize = 2;
static GLUE_CP_SEM: Lazy<Semaphore> =
    Lazy::new(|| Semaphore::new(DEFAULT_GLUE_CONTROL_PLANE_CONCURRENCY));
static ATHENA_WG_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));
static NAMESPACE_LOCKS: Lazy<DashMap<String, Arc<Mutex<()>>>> = Lazy::new(|| DashMap::new());

fn get_namespace_lock(namespace: &str) -> Arc<Mutex<()>> {
    NAMESPACE_LOCKS
        .entry(namespace.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
pub struct DataSinkAthenaPluginConfig {
    pub format: Option<String>,
    // pub batch_size_seconds: Option<i64>,
    // pub batch_size_bytes: Option<i64>,
    pub s3_bucket: String,
    pub s3_prefix: String,
    // pub time_bucket: Option<String>,
    pub athena_workgroup_name: String,
    #[serde(default)]
    pub glue_database_name: String,
    pub athena_results_s3_bucket: String,
}

impl TryFrom<DataSinkPluginConfig> for DataSinkAthenaPluginConfig {
    type Error = String;

    fn try_from(entry: DataSinkPluginConfig) -> Result<Self, Self::Error> {
        entry.decode_for_plugin("Athena")
    }
}

pub struct DataSinkAthenaPlugin {
    s3_client: S3Client,
    #[allow(dead_code)]
    athena_client: AthenaClient,
    #[allow(dead_code)]
    buffer_name: String,
    context: RuntimeExecutionContext,
    binding: RuntimeBinding,
    config: DataSinkAthenaPluginConfig,
    // s3_bucket: String,
    // s3_prefix: String,
    // time_bucket: String,
    #[allow(dead_code)]
    max_async_uploads: i64,
    upload_sem: Arc<Semaphore>,
    deadletter_schema_ready: AtomicBool,
    schema_state: RwLock<InstalledAthenaSchemaState>,
}

skippr_runtime_sdk::declare_sink_spec!(
    AthenaSinkSpec,
    DataSinkAthenaPlugin,
    skippr_runtime_sdk::plugins::cdc::sink_capabilities::ATHENA,
    skippr_runtime_sdk::plugins::DeterministicObjectOverwrite
);

#[derive(Debug, Default)]
struct InstalledAthenaSchemaState {
    version: u64,
    namespaces: BTreeMap<String, OutputMetadata>,
}

fn is_deadletter_athena_target(binding: RuntimeBinding, namespace: &str) -> bool {
    binding == RuntimeBinding::Deadletter
        && namespace == skippr_runtime_sdk::sink_compat::deadletter::table_name()
}

fn deadletter_output_metadata() -> OutputMetadata {
    serde_json::from_value(serde_json::json!({
        "out_field_name": "",
        "determined_type": "record",
        "determined_type_values": "",
        "fields": {
            "id": {
                "out_field_name": "id",
                "determined_type": "string",
                "determined_type_values": "",
                "fields": {}
            },
            "namespace": {
                "out_field_name": "namespace",
                "determined_type": "string",
                "determined_type_values": "",
                "fields": {}
            },
            "record": {
                "out_field_name": "record",
                "determined_type": "string",
                "determined_type_values": "",
                "fields": {}
            },
            "error": {
                "out_field_name": "error",
                "determined_type": "string",
                "determined_type_values": "",
                "fields": {}
            },
            "failure_code": {
                "out_field_name": "failure_code",
                "determined_type": "string",
                "determined_type_values": "",
                "fields": {}
            },
            "event_time": {
                "out_field_name": "event_time",
                "determined_type": "long",
                "determined_type_values": "",
                "fields": {}
            },
            "processed_time": {
                "out_field_name": "processed_time",
                "determined_type": "long",
                "determined_type_values": "",
                "fields": {}
            },
            "source_uri": {
                "out_field_name": "source_uri",
                "determined_type": "string",
                "determined_type_values": "",
                "fields": {}
            },
            "offset_key": {
                "out_field_name": "offset_key",
                "determined_type": "string",
                "determined_type_values": "",
                "fields": {}
            },
            "offset_pos": {
                "out_field_name": "offset_pos",
                "determined_type": "long",
                "determined_type_values": "",
                "fields": {}
            }
        }
    }))
    .expect("deadletter metadata shape must remain valid")
}

#[async_trait]
impl DataSink for DataSinkAthenaPlugin {
    async fn sync(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
        cdc_ctx: Option<&skippr_runtime_sdk::plugins::cdc::SyncContext>,
    ) -> Result<(), std::io::Error> {
        self.sync_with_context(
            stream,
            SinkWriteContext {
                filename,
                compaction_id: String::new(),
                idempotency_key: String::new(),
                wal_refs: Vec::new(),
                write_semantics: skippr_runtime_sdk::plugins::SinkWriteSemantics::AtLeastOnce,
                schema_fingerprint: String::new(),
                cdc_ctx,
                source_contract: None,
            },
        )
        .await
    }

    async fn sync_with_context(
        &self,
        stream: SendableRecordBatchStream,
        ctx: SinkWriteContext<'_>,
    ) -> Result<(), std::io::Error> {
        let stream = match ctx.cdc_ctx {
            Some(cdc) => super::cdc_encode::augment_stream_with_cdc_columns(stream, &cdc.part_meta),
            None => stream,
        };
        let namespace = BufferChunker::decode_file_namespace(&ctx.filename);
        let resolved_contract = if ctx.cdc_ctx.is_some() {
            None
        } else {
            ctx.source_contract
                .cloned()
                .or_else(|| namespace_source_contract(&namespace))
        };
        let write_policy = if ctx.cdc_ctx.is_some() {
            WritePolicy::Append
        } else {
            resolved_contract
                .as_ref()
                .map(|c| c.write_policy)
                .unwrap_or(WritePolicy::Append)
        };
        if ctx.cdc_ctx.is_none() {
            if let Some(ref contract) = resolved_contract {
                validate_write_policy_for_sink(contract, "Athena", ATHENA_WRITE_POLICY_SUPPORT)
                    .map_err(|err| io::Error::new(io::ErrorKind::Unsupported, err.to_string()))?;
            }
            ensure_source_contract_for_policy(
                &namespace,
                write_policy,
                resolved_contract.as_ref(),
            )?;
        }
        self.inner_sync(
            stream,
            ctx.filename,
            write_policy,
            resolved_contract.as_ref(),
            if ctx.idempotency_key.is_empty() {
                None
            } else {
                Some(ctx.idempotency_key.as_str())
            },
        )
        .await
    }

    fn capability(&self) -> &'static skippr_runtime_sdk::plugins::cdc::SinkCapability {
        &skippr_runtime_sdk::plugins::cdc::sink_capabilities::ATHENA
    }

    async fn install_schema_state(
        &self,
        schema_version: u64,
        namespaces: &BTreeMap<String, OutputMetadata>,
    ) -> Result<(), std::io::Error> {
        let mut guard = self.schema_state.write().await;
        guard.version = schema_version;
        guard.namespaces = namespaces.clone();
        Ok(())
    }
}

#[async_trait]
impl SchemaSink for DataSinkAthenaPlugin {
    async fn sync_schema(
        &self,
        namespace: &str,
        metadata: &OutputMetadata,
    ) -> Result<(), io::Error> {
        self.sync_schema_request(
            SchemaSyncRequest {
                namespace,
                compaction_id: "",
                source_contract: None,
            },
            metadata,
        )
        .await
    }

    async fn sync_schema_request(
        &self,
        request: SchemaSyncRequest<'_>,
        metadata: &OutputMetadata,
    ) -> Result<(), io::Error> {
        let source_contract = request
            .source_contract
            .cloned()
            .or_else(|| namespace_source_contract(request.namespace));
        AwsAthena::create_or_update_schema_with_config(
            request.namespace,
            metadata,
            &self.context,
            self.binding,
            self.config.clone(),
            source_contract.as_ref(),
        )
        .await
    }
}

impl DataSinkAthenaPlugin {
    pub async fn new_with_config(
        context: RuntimeExecutionContext,
        binding: RuntimeBinding,
        buffer_name: String,
        athena_config: DataSinkAthenaPluginConfig,
    ) -> DataSinkAthenaPlugin {
        let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .load()
            .await;

        let s3_client = S3Client::new(&aws_config);
        let athena_client = AthenaClient::new(&aws_config);
        let tuned_uploads = skippr_runtime_sdk::metrics::counters::UPLOAD_CONCURRENCY_TARGET
            .load(std::sync::atomic::Ordering::Relaxed);
        let max_async_uploads = tuned_uploads.max(1);

        Self {
            s3_client,
            athena_client,
            context,
            binding,
            config: athena_config,
            buffer_name: buffer_name,
            max_async_uploads: max_async_uploads as i64,
            upload_sem: Arc::new(Semaphore::new(max_async_uploads)),
            deadletter_schema_ready: AtomicBool::new(false),
            schema_state: RwLock::new(InstalledAthenaSchemaState::default()),
        }
    }

    async fn namespace_metadata(&self, namespace: &str) -> Result<OutputMetadata, io::Error> {
        let guard = self.schema_state.read().await;
        guard.namespaces.get(namespace).cloned().ok_or_else(|| {
            io::Error::other(format!(
                "Athena sink schema state v{} is missing namespace '{}'",
                guard.version, namespace
            ))
        })
    }

    async fn delete_s3_prefix(&self, bucket: &str, prefix: &str) -> Result<(), io::Error> {
        let lock = get_namespace_lock(prefix);
        let _guard = lock.lock().await;
        let mut next_token = None;
        loop {
            let resp = self
                .s3_client
                .list_objects_v2()
                .bucket(bucket)
                .prefix(prefix)
                .max_keys(1000)
                .set_continuation_token(next_token.clone())
                .send()
                .await
                .map_err(|e| io::Error::other(e.to_string()))?;
            let mut delete_objects = Vec::new();
            for obj in resp.contents() {
                if let Some(key) = obj.key() {
                    delete_objects.push(
                        ObjectIdentifier::builder()
                            .key(key)
                            .build()
                            .map_err(|e| io::Error::other(e.to_string()))?,
                    );
                }
            }
            if !delete_objects.is_empty() {
                self.s3_client
                    .delete_objects()
                    .bucket(bucket)
                    .delete(
                        Delete::builder()
                            .set_objects(Some(delete_objects))
                            .build()
                            .map_err(|e| io::Error::other(e.to_string()))?,
                    )
                    .send()
                    .await
                    .map_err(|e| io::Error::other(e.to_string()))?;
            }
            next_token = resp.next_continuation_token;
            if next_token.is_none() {
                break;
            }
        }
        Ok(())
    }

    pub async fn inner_sync(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
        write_policy: WritePolicy,
        source_contract: Option<&SourceNamespaceContract>,
        object_stem: Option<&str>,
    ) -> Result<(), std::io::Error> {
        let _bucket = &self.config.s3_bucket;
        let key = &self.config.s3_prefix;

        let namespace = BufferChunker::decode_file_namespace(&filename);
        let is_deadletter_target = is_deadletter_athena_target(self.binding, &namespace);

        if is_deadletter_target && !self.deadletter_schema_ready.load(Ordering::Acquire) {
            AwsAthena::create_or_update_schema_with_config(
                &namespace,
                &deadletter_output_metadata(),
                &self.context,
                self.binding,
                self.config.clone(),
                None,
            )
            .await?;
            self.deadletter_schema_ready.store(true, Ordering::Release);
        }

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

        // Partitioning (contract keys first to match Glue table partition key order).
        let mut partition_values: Vec<String> = vec![];

        let (contract_peek_batch, stream) =
            if write_policy == WritePolicy::ReplacePartition && source_contract.is_some() {
                Self::peek_first_batch(stream).await?
            } else {
                (None, stream)
            };

        if let (Some(contract), Some(batch)) = (source_contract, contract_peek_batch.as_ref()) {
            if !contract.partition_key.is_empty() {
                for (column, value) in contract_partition_key_values(contract, batch)? {
                    partition_values.push(value.clone());
                    tags.insert(column.clone(), value.clone());
                    full_key = format!("{}/{}={}", full_key, column, value);
                }
            }
        }

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

        if !is_deadletter_target && !time_partition_str.is_empty() {
            match AwsAthena::time_partition_values_for_layout(
                &filename,
                &self.context.output_layout,
            ) {
                Ok(time_partition_values) => {
                    let granularity_names =
                        AwsAthena::time_partition_names_for_layout(&self.context.output_layout);
                    partition_values.extend(time_partition_values.iter().map(|v| v.to_string()));
                    granularity_names
                        .iter()
                        .enumerate()
                        .for_each(|(i, granularity)| {
                            full_key = format!(
                                "{}/{}={}",
                                full_key, granularity, time_partition_values[i]
                            );
                        });
                }
                Err(e) => {
                    let contract_covers_partition =
                        source_contract.is_some_and(|contract| !contract.partition_key.is_empty());
                    if write_policy == WritePolicy::ReplacePartition
                        && !contract_covers_partition
                        && partition_path.is_empty()
                        && partition_values.is_empty()
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!(
                                "Athena ReplacePartition for namespace '{}' requires time partitions in filename or source contract partition_key: {}",
                                namespace, e
                            ),
                        ));
                    }
                    warn!(
                        "Failed to derive time partitions from filename '{}': {}. Proceeding without time partitions.",
                        filename, e
                    );
                }
            }
        }

        let partition_metadata = if !is_deadletter_target && !partition_values.is_empty() {
            Some(self.namespace_metadata(&namespace).await?)
        } else {
            None
        };
        let has_contract_partition_scope = source_contract
            .zip(contract_peek_batch.as_ref())
            .is_some_and(|(contract, _)| !contract.partition_key.is_empty());
        let has_partition_scope = !partition_path.is_empty()
            || !time_partition_str.is_empty()
            || has_contract_partition_scope;

        match write_policy {
            WritePolicy::ReplacePartition => {
                if full_key.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "Athena ReplacePartition for namespace '{}' requires a non-empty object prefix",
                            namespace
                        ),
                    ));
                }
                let delete_prefix = if let (Some(contract), Some(batch)) =
                    (source_contract, contract_peek_batch.as_ref())
                {
                    contract_partition_delete_prefix(&namespace, trimmed_key, contract, batch)?
                } else if has_partition_scope {
                    full_key.clone()
                } else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "Athena ReplacePartition for namespace '{}' requires partition scope \
                             from source contract or encoded filename partitions",
                            namespace
                        ),
                    ));
                };
                info!(
                    "Athena ReplacePartition: deleting s3://{}/{} before upload",
                    self.config.s3_bucket, delete_prefix
                );
                self.delete_s3_prefix(&self.config.s3_bucket, &delete_prefix)
                    .await?;
            }
            WritePolicy::ReplaceTable => {
                if namespace.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "Athena ReplaceTable requires a non-empty namespace",
                    ));
                }
                let table_prefix = if trimmed_key.is_empty() {
                    namespace.clone()
                } else {
                    format!("{}/{}", trimmed_key, namespace)
                };
                info!(
                    "Athena ReplaceTable: deleting s3://{}/{} before upload",
                    self.config.s3_bucket, table_prefix
                );
                self.delete_s3_prefix(&self.config.s3_bucket, &table_prefix)
                    .await?;
            }
            WritePolicy::MergeByKey => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!(
                        "Athena sink does not support MergeByKey for namespace '{}'",
                        namespace
                    ),
                ));
            }
            WritePolicy::Append => {}
        }

        // Use grouped idempotency keys directly; legacy writes keep hashed filenames.
        let object_stem = object_stem
            .map(str::to_string)
            .unwrap_or_else(|| hex::encode(md5::compute(&filename).0));
        let final_key = format!("{}/{}.parquet", full_key, object_stem);

        // Prepare S3 tagging string
        let tags_str = tags
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<String>>()
            .join("&");

        // Resize semaphore if target changed dynamically (clamp to 1..256)
        {
            let target = skippr_runtime_sdk::metrics::counters::UPLOAD_CONCURRENCY_TARGET
                .load(std::sync::atomic::Ordering::Relaxed)
                .clamp(1, 256);
            let current = self.upload_sem.available_permits() + 1; // approx
            if target as usize != current {
                if target as usize > current {
                    self.upload_sem.add_permits(target as usize - current);
                }
            }
        }
        let _permit = self
            .upload_sem
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "Semaphore closed"))?;
        let upload_start = std::time::Instant::now();
        skippr_runtime_sdk::metrics::counters::inc_uploads_in_flight();

        let bucket = self.config.s3_bucket.clone();

        // Stream Parquet to S3 via multipart upload
        debug!(
            "Uploader: start ns={} key_base={} filename={} bucket={}",
            namespace, full_key, filename, bucket
        );

        let key_for_upload = final_key.clone();

        // Initiate multipart upload with timeout
        let create_out = match tokio::time::timeout(
            std::time::Duration::from_secs(120),
            self.s3_client
                .create_multipart_upload()
                .bucket(&bucket)
                .key(&key_for_upload)
                .content_type("application/octet-stream")
                .tagging(tags_str)
                .send(),
        )
        .await
        {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                skippr_runtime_sdk::metrics::counters::dec_uploads_in_flight();
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!(
                        "Failed to initiate multipart upload: {}",
                        e.into_service_error()
                    ),
                ));
            }
            Err(_) => {
                skippr_runtime_sdk::metrics::counters::dec_uploads_in_flight();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Timeout initiating multipart upload",
                ));
            }
        };
        let upload_id = create_out.upload_id().unwrap_or("").to_string();
        if upload_id.is_empty() {
            skippr_runtime_sdk::metrics::counters::dec_uploads_in_flight();
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "Missing upload_id from S3",
            ));
        }

        // Writer that uploads parts as bytes are produced
        struct MultipartWriter {
            client: S3Client,
            bucket: String,
            key: String,
            upload_id: String,
            part_size: usize,
            buffer: Vec<u8>,
            next_part: i32,
            parts: Vec<CompletedPart>,
            total_bytes: u64,
        }

        impl MultipartWriter {
            fn new(
                client: S3Client,
                bucket: String,
                key: String,
                upload_id: String,
                part_size: usize,
            ) -> Self {
                Self {
                    client,
                    bucket,
                    key,
                    upload_id,
                    part_size: part_size.max(5 * 1024 * 1024),
                    buffer: Vec::with_capacity(part_size.max(5 * 1024 * 1024)),
                    next_part: 1,
                    parts: Vec::new(),
                    total_bytes: 0,
                }
            }

            fn upload_chunk_blocking(&mut self, chunk: Vec<u8>) -> io::Result<()> {
                let client = self.client.clone();
                let bucket = self.bucket.clone();
                let key = self.key.clone();
                let upload_id = self.upload_id.clone();
                let part_number = self.next_part;
                self.next_part += 1;
                self.total_bytes += chunk.len() as u64;
                // println!(
                //     "uploading part {} for {} (size={}, total={})",
                //     part_number,
                //     key,
                //     Helpers::human_readable_size((chunk.len()) as u64),
                //     Helpers::human_readable_size(self.total_bytes)
                // );
                block_in_place(|| {
                    let body = ByteStream::from(Bytes::from(chunk));
                    let fut = async move {
                        tokio::time::timeout(
                            std::time::Duration::from_secs(300),
                            client
                                .upload_part()
                                .bucket(bucket)
                                .key(key)
                                .upload_id(upload_id)
                                .part_number(part_number)
                                .body(body)
                                .send(),
                        )
                        .await
                    };
                    match tokio::runtime::Handle::current().block_on(fut) {
                        Ok(Ok(resp)) => {
                            let etag = resp.e_tag().unwrap_or("").to_string();
                            let part = CompletedPart::builder()
                                .e_tag(etag)
                                .part_number(part_number)
                                .build();
                            self.parts.push(part);
                            Ok(())
                        }
                        Ok(Err(e)) => Err(io::Error::new(
                            io::ErrorKind::Other,
                            format!("upload_part failed: {}", e.into_service_error()),
                        )),
                        Err(_) => Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "upload_part timed out",
                        )),
                    }
                })
            }

            fn flush_full_parts(&mut self) -> io::Result<()> {
                while self.buffer.len() >= self.part_size {
                    let chunk = self.buffer.drain(..self.part_size).collect::<Vec<u8>>();
                    self.upload_chunk_blocking(chunk)?;
                }
                Ok(())
            }

            fn complete(&mut self) -> io::Result<u64> {
                // Upload remaining as final part (can be < 5MiB)
                if !self.buffer.is_empty() {
                    let chunk = std::mem::take(&mut self.buffer);
                    self.upload_chunk_blocking(chunk)?;
                }
                // Complete multipart upload
                let client = self.client.clone();
                let bucket = self.bucket.clone();
                let key = self.key.clone();
                let upload_id = self.upload_id.clone();
                let parts = self.parts.clone();
                debug!(
                    "completing multipart upload for {} (parts={}, total={})",
                    key,
                    parts.len(),
                    Helpers::human_readable_size(self.total_bytes)
                );
                block_in_place(|| {
                    let fut = async move {
                        tokio::time::timeout(
                            std::time::Duration::from_secs(600),
                            client
                                .complete_multipart_upload()
                                .bucket(bucket)
                                .key(key)
                                .upload_id(upload_id)
                                .multipart_upload(
                                    CompletedMultipartUpload::builder()
                                        .set_parts(Some(parts))
                                        .build(),
                                )
                                .send(),
                        )
                        .await
                    };
                    match tokio::runtime::Handle::current().block_on(fut) {
                        Ok(Ok(_)) => Ok(()),
                        Ok(Err(e)) => Err(io::Error::new(
                            io::ErrorKind::Other,
                            format!(
                                "complete_multipart_upload failed: {}",
                                e.into_service_error()
                            ),
                        )),
                        Err(_) => Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "complete_multipart_upload timed out",
                        )),
                    }
                })?;
                Ok(self.total_bytes)
            }

            fn abort(&self) {
                let client = self.client.clone();
                let bucket = self.bucket.clone();
                let key = self.key.clone();
                let upload_id = self.upload_id.clone();
                let _ = block_in_place(|| {
                    let fut = async move {
                        client
                            .abort_multipart_upload()
                            .bucket(bucket)
                            .key(key)
                            .upload_id(upload_id)
                            .send()
                            .await
                    };
                    tokio::runtime::Handle::current().block_on(fut).map(|_| ())
                });
            }
        }

        impl std::io::Write for MultipartWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.buffer.extend_from_slice(buf);
                // Upload any full parts
                self.flush_full_parts()?;
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                // No-op; completion will upload the tail
                Ok(())
            }
        }

        // Materialize batches from stream
        let schema = stream.schema();
        let mut raw_batches: Vec<arrow::array::RecordBatch> = Vec::new();
        let mut rows_written: u64 = 0;
        let mut batches_stream = stream;
        while let Some(batch_res) = batches_stream.next().await {
            let batch = batch_res.map_err(|e| {
                io::Error::new(io::ErrorKind::Other, format!("Stream error: {}", e))
            })?;
            rows_written += batch.num_rows() as u64;
            raw_batches.push(batch);
            tokio::task::yield_now().await;
        }

        // Resolve ordering and sort if configured
        let order_fields =
            skippr_runtime_sdk::converters::parquet_ordering::resolve_effective_order_from_fields(
                &schema,
                &self.context.output_layout.order_fields,
            );
        let sorted_batches =
            skippr_runtime_sdk::converters::parquet_ordering::materialize_and_sort(
                raw_batches,
                &schema,
                &order_fields,
            )?;

        let row_group_size =
            skippr_runtime_sdk::converters::parquet_ordering::estimate_row_group_size(
                &sorted_batches,
                &order_fields,
            );
        let props = skippr_runtime_sdk::converters::parquet_ordering::build_writer_properties(
            &schema,
            &order_fields,
            row_group_size,
        );

        let mut writer = MultipartWriter::new(
            self.s3_client.clone(),
            bucket.clone(),
            key_for_upload.clone(),
            upload_id.clone(),
            64 * 1024 * 1024,
        );

        let mut parquet_writer =
            ArrowWriter::try_new(&mut writer, schema, Some(props)).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::Other,
                    format!("Failed to init ArrowWriter: {}", e),
                )
            })?;

        for (batch_index, batch) in sorted_batches.iter().enumerate() {
            parquet_writer.write(batch).map_err(|e| {
                io::Error::new(io::ErrorKind::Other, format!("Parquet write error: {}", e))
            })?;
            debug!(
                "Uploader: wrote batch idx={} rows={} key={}",
                batch_index,
                batch.num_rows(),
                key_for_upload
            );
        }

        let _meta = parquet_writer.close().map_err(|e| {
            io::Error::new(io::ErrorKind::Other, format!("Parquet close error: {}", e))
        })?;

        let uploaded_bytes = match writer.complete() {
            Ok(sz) => sz,
            Err(e) => {
                // Best-effort abort
                writer.abort();
                skippr_runtime_sdk::metrics::counters::dec_uploads_in_flight();
                return Err(e);
            }
        };
        let partition_summary = if partition_values.is_empty() {
            format!("s3://{}/{}/", bucket, full_key.trim_end_matches('/'))
        } else {
            format!(
                "s3://{}/{}/ values={:?}",
                bucket,
                full_key.trim_end_matches('/'),
                partition_values
            )
        };
        info!(
            "Uploaded s3://{}/{} to S3 (rows={}, bytes={}, namespace={}, partitions={})",
            bucket, final_key, rows_written, uploaded_bytes, namespace, partition_summary
        );
        debug!(
            "Uploader: complete key={} upload_id={} parts={} total_bytes={} rows={}",
            key_for_upload,
            upload_id,
            writer.parts.len(),
            uploaded_bytes,
            rows_written
        );
        metrics_counters::add_parquet_bytes(uploaded_bytes);
        metrics_counters::add_parquet_objects(1);
        metrics_counters::add_parquet_rows(rows_written);
        skippr_runtime_sdk::metrics::counters::add_upload(1);
        skippr_runtime_sdk::metrics::counters::add_upload_latency_ns(
            upload_start.elapsed().as_nanos() as u64,
        );
        skippr_runtime_sdk::metrics::counters::dec_uploads_in_flight();
        // Update manifest with the canonical namespace root prefix (absolute s3:// URL)
        // Canonical: s3://{bucket}/{s3_prefix}/{namespace}/
        let trimmed_key_root = key.trim_matches('/').to_string();
        let ns_root = if !namespace.is_empty() {
            if trimmed_key_root.is_empty() {
                namespace.clone()
            } else {
                format!("{}/{}", trimmed_key_root, namespace)
            }
        } else {
            trimmed_key_root.clone()
        };
        let mut abs_prefix = format!("s3://{}/{}", bucket, ns_root.trim_start_matches('/'));
        if !abs_prefix.ends_with('/') {
            abs_prefix.push('/');
        }
        {
            let ns = namespace.to_string();
            let prefix_for_manifest = abs_prefix.clone();
            let pipeline_name = self.context.pipeline_name.clone();
            tokio::spawn(async move {
                crate::helpers::manifest::Manifest::ensure_prefix_and_db(
                    &ns,
                    &prefix_for_manifest,
                    &pipeline_name,
                )
                .await;
            });
        }

        if let Some(partition_metadata) = partition_metadata {
            AwsAthena::glue_create_partition(
                &self.context,
                self.binding,
                &self.config,
                &namespace,
                partition_values,
                &full_key,
                &partition_metadata,
                source_contract,
            )
            .await
            .map_err(io::Error::other)?;
        }
        Ok(())
    }

    #[allow(dead_code)]
    async fn upload_object(
        _client: S3Client,
        _bucket: String,
        _key: String,
        stream: SendableRecordBatchStream,
        tag_hashmap: HashMap<String, String>,
    ) -> Result<(), std::io::Error> {
        let _tags = tag_hashmap
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<String>>()
            .join("&");

        // Serialize to parquet asynchronously
        let parquet = serialize_to_parquet(stream).await?;

        // Create the upload body stream
        let _body = ByteStream::from(parquet.bytes);

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
                    if s.contains("Throttling")
                        || s.contains("TooManyRequests")
                        || s.contains("ConcurrentModification")
                        || s == "RETRY_TRANSIENT"
                    {
                        attempt += 1;
                        if attempt > 6 {
                            return Err(s);
                        }
                        let base = 200u64 * (1u64 << attempt.min(6));
                        let jitter: u64 = rand::thread_rng().gen_range(0..100);
                        let delay_ms = base + jitter;
                        warn!(
                            "Glue/Athena {} retry {} in {}ms: {}",
                            op_name, attempt, delay_ms, s
                        );
                        tokio_sleep(TokioDuration::from_millis(delay_ms)).await;
                        continue;
                    }
                    return Err(s);
                }
            }
        }
    }
    pub async fn create_or_update_schema_with_config(
        namespace: &str,
        schema: &OutputMetadata,
        context: &RuntimeExecutionContext,
        binding: RuntimeBinding,
        config: DataSinkAthenaPluginConfig,
        source_contract: Option<&SourceNamespaceContract>,
    ) -> io::Result<()> {
        // Serialize workgroup changes to avoid Athena InvalidRequestException on concurrent updates
        let _wg_guard = ATHENA_WG_LOCK.lock().await;
        match AwsAthena::get_work_group(&config).await {
            Ok(true) => {}
            Ok(false) => {}
            Err(_err) => match AwsAthena::create_workgroup(&config).await {
                Ok(_) => {
                    info!("Created Athena Workgroup");
                }
                Err(_err) => match AwsAthena::update_workgroup(&config).await {
                    Ok(_) => {
                        info!("Updated Athena Workgroup");
                    }
                    Err(err) => {
                        return Err(io::Error::other(format!(
                            "failed to create/update Athena workgroup '{}': {}",
                            config.athena_workgroup_name, err
                        )));
                    }
                },
            },
        }
        drop(_wg_guard);

        // Limit Glue control-plane concurrency globally
        let _cp_permit = GLUE_CP_SEM.acquire().await.unwrap();

        // Serialize by namespace to avoid ConcurrentModificationException
        let ns_lock = get_namespace_lock(namespace);
        let _ns_guard = ns_lock.lock().await;

        match AwsAthena::glue_get_database(&config).await {
            Ok(true) => {}
            Ok(false) | Err(_) => {
                // Create database with backoff; AlreadyExists => success
                AwsAthena::backoff_retry(
                    || AwsAthena::glue_create_database(&config),
                    "create_database",
                )
                .await
                .map_err(|err| {
                    io::Error::other(format!(
                        "failed to create Glue database '{}': {}",
                        config.glue_database_name, err
                    ))
                })?;
            }
        }

        match AwsAthena::glue_get_table(&config, namespace).await {
            Ok(table) => {
                let deadletter_table_needs_rebuild =
                    is_deadletter_athena_target(binding, namespace)
                        && table
                            .table()
                            .and_then(|t| t.partition_keys.as_ref())
                            .is_some_and(|keys| !keys.is_empty());
                if deadletter_table_needs_rebuild {
                    info!(
                        "Rebuilding deadletter Glue table '{}' in database '{}' without partitions",
                        namespace, config.glue_database_name
                    );
                    AwsAthena::backoff_retry(
                        || AwsAthena::glue_delete_table(&config, namespace),
                        "delete_table",
                    )
                    .await
                    .map_err(|err| {
                        io::Error::other(format!(
                            "failed to delete misconfigured deadletter Glue table '{}.{}': {}",
                            config.glue_database_name, namespace, err
                        ))
                    })?;
                    AwsAthena::backoff_retry(
                        || {
                            AwsAthena::glue_create_table(
                                context,
                                binding,
                                &config,
                                namespace,
                                schema,
                                source_contract,
                            )
                        },
                        "create_table",
                    )
                    .await
                    .map_err(|err| {
                        io::Error::other(format!(
                            "failed to recreate deadletter Glue table '{}.{}': {}",
                            config.glue_database_name, namespace, err
                        ))
                    })?;
                    return Ok(());
                }
                AwsAthena::backoff_retry(
                    || AwsAthena::glue_update_table(&config, namespace, schema, table.clone()),
                    "update_table",
                )
                .await
                .map_err(|err| {
                    io::Error::other(format!(
                        "failed to update Glue table '{}.{}': {}",
                        config.glue_database_name, namespace, err
                    ))
                })?;
            }
            Err(SdkError::ServiceError(err))
                if matches!(err.err(), GetTableError::EntityNotFoundException(_)) =>
            {
                AwsAthena::backoff_retry(
                    || {
                        AwsAthena::glue_create_table(
                            context,
                            binding,
                            &config,
                            namespace,
                            schema,
                            source_contract,
                        )
                    },
                    "create_table",
                )
                .await
                .map_err(|err| {
                    io::Error::other(format!(
                        "failed to create Glue table '{}.{}': {}",
                        config.glue_database_name, namespace, err
                    ))
                })?;
            }
            Err(err) => {
                return Err(io::Error::other(format!(
                    "failed to read Glue table '{}.{}': {}",
                    config.glue_database_name, namespace, err
                )));
            }
        }
        Ok(())
    }

    pub async fn get_work_group(config: &DataSinkAthenaPluginConfig) -> Result<bool, String> {
        let workgroup = config.athena_workgroup_name.clone();
        let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .load()
            .await;

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

    pub async fn glue_get_database(config: &DataSinkAthenaPluginConfig) -> Result<bool, String> {
        let database_name = config.glue_database_name.clone();

        let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .load()
            .await;

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

    pub async fn glue_get_table(
        config: &DataSinkAthenaPluginConfig,
        namespace: &str,
    ) -> Result<GetTableOutput, SdkError<GetTableError>> {
        let database_name = config.glue_database_name.clone();
        let table_name = namespace.to_string();

        let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .load()
            .await;

        let glue_client = GlueClient::new(&aws_config);

        glue_client
            .get_table()
            .database_name(&database_name)
            .name(&table_name)
            .send()
            .await
    }

    pub async fn create_workgroup(config: &DataSinkAthenaPluginConfig) -> Result<bool, String> {
        let workgroup = config.athena_workgroup_name.clone();
        let bucket = config.athena_results_s3_bucket.clone();
        let path = config.s3_prefix.clone();
        let path = path.trim_matches('/');

        let path = std::path::Path::new(&bucket)
            .join(&path)
            .join("query-results")
            .to_str()
            .unwrap()
            .to_string();

        let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .load()
            .await;

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

    pub async fn update_workgroup(config: &DataSinkAthenaPluginConfig) -> Result<bool, String> {
        let workgroup = config.athena_workgroup_name.clone();
        let bucket = config.athena_results_s3_bucket.clone();
        let path = config.s3_prefix.clone();
        let path = path.trim_matches('/');

        let path = std::path::Path::new(&bucket)
            .join(&path)
            .join("query-results")
            .to_str()
            .unwrap()
            .to_string();

        let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .load()
            .await;

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

    pub async fn glue_create_database(config: &DataSinkAthenaPluginConfig) -> Result<bool, String> {
        let database = config.glue_database_name.clone();
        let bucket = config.s3_bucket.clone();
        let path = config.s3_prefix.clone();
        let path = path.trim_matches('/');

        let path = std::path::Path::new(&bucket)
            .join(&path)
            .to_str()
            .unwrap()
            .to_string();

        let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .load()
            .await;

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

                println!(
                    "Waiting {} seconds before dropping database '{}', ctrl+c to cancel",
                    timeout, database_name
                );

                loop {
                    if timeout > 0 {
                        tokio::time::sleep(tokio::time::Duration::from_secs(timeout)).await;
                        timeout -= 1;
                    } else {
                        break;
                    }
                }

                break;
            } else if input.trim().eq_ignore_ascii_case("exit") {
                // println!("Drop canceled. Exiting without dropping database.");
                return Err(format!(
                    "Drop canceled. Exiting without dropping database '{}'.",
                    database_name
                ));
            } else {
                // println!("Incorrect database name. Please try again, or type 'exit' to cancel.");
                return Err(format!(
                    "Incorrect database name entered: '{}'.",
                    input.trim()
                ));
                // The loop will continue, prompting the user again
            }
        }

        let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .load()
            .await;

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

                        println!(
                            "Deleting {} partitions in table '{}'",
                            partitions.len(),
                            table_name
                        );

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
        let location_uri = glue_client
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
        let path = location_uri
            .split('/')
            .skip(3)
            .collect::<Vec<&str>>()
            .join("/");

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
            .await
        {
            let mut delete_objects: Vec<ObjectIdentifier> = vec![];

            if resp.contents.is_none() {
                continue;
            };

            let objects = resp.contents();

            for obj in objects {
                let obj_id = ObjectIdentifier::builder().set_key(obj.key.clone()).build();

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
                println!(
                    "Deleting {} S3 objects from bucket {}",
                    delete_objects.len(),
                    bucket
                );

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
                    .await
                    .unwrap();
            }

            if resp.next_continuation_token.is_none() {
                break;
            }
            next_token = resp.next_continuation_token;
        }

        Ok(true)
    }

    fn get_partition_by_fields(
        output_layout: &skippr_runtime_sdk::protocol::RuntimeOutputLayout,
        partitions: &mut Vec<Column>,
    ) {
        for field_dot in &output_layout.partition_fields {
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

    fn time_partition_name_for_layout(
        output_layout: &skippr_runtime_sdk::protocol::RuntimeOutputLayout,
        granularity: &str,
    ) -> String {
        match output_layout.time_partition_prefix.as_deref() {
            Some(prefix) if !prefix.is_empty() => format!("{prefix}{granularity}"),
            _ => granularity.to_string(),
        }
    }

    fn time_partition_names_for_layout(
        output_layout: &skippr_runtime_sdk::protocol::RuntimeOutputLayout,
    ) -> Vec<String> {
        let Some(target) = output_layout.time_partition_granularity.as_deref() else {
            return Vec::new();
        };
        if target.trim().is_empty() {
            return Vec::new();
        }

        let mut names = Vec::new();
        for granularity in TIME_PARTITION_GRANULARITIES {
            names.push(Self::time_partition_name_for_layout(
                output_layout,
                granularity,
            ));
            if granularity.eq_ignore_ascii_case(target) {
                return names;
            }
        }
        names
    }

    fn time_partition_values_for_layout(
        filename: &String,
        output_layout: &skippr_runtime_sdk::protocol::RuntimeOutputLayout,
    ) -> Result<Vec<u32>, io::Error> {
        let Some(target) = output_layout.time_partition_granularity.as_deref() else {
            return Ok(Vec::new());
        };
        if target.trim().is_empty() {
            return Ok(Vec::new());
        }

        let partitioner = TimePartitioner::new(filename);
        let time_partition_str = BufferChunker::decode_file_time_to_datetime_string(filename);
        if time_partition_str.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Time partition string is empty",
            ));
        }
        let date = partitioner.parse_datetime(&time_partition_str)?;

        let mut values = Vec::new();
        for granularity in TIME_PARTITION_GRANULARITIES {
            values.push(TimePartitioner::get_date_component(date, granularity)?);
            if granularity.eq_ignore_ascii_case(target) {
                return Ok(values);
            }
        }

        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Unsupported time partition granularity '{target}'"),
        ))
    }

    pub async fn glue_delete_table(
        config: &DataSinkAthenaPluginConfig,
        namespace: &str,
    ) -> Result<bool, String> {
        let database = config.glue_database_name.clone();
        let table_name = namespace.to_string();

        let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .load()
            .await;

        let glue_client = GlueClient::new(&aws_config);

        match glue_client
            .delete_table()
            .database_name(&database)
            .name(&table_name)
            .send()
            .await
        {
            Ok(_output) => Ok(true),
            Err(err) => Err(err.into_service_error().to_string()),
        }
    }

    pub async fn glue_create_table(
        context: &RuntimeExecutionContext,
        binding: RuntimeBinding,
        config: &DataSinkAthenaPluginConfig,
        namespace: &str,
        metadata: &OutputMetadata,
        source_contract: Option<&SourceNamespaceContract>,
    ) -> Result<bool, String> {
        let database = config.glue_database_name.clone();
        let table_name = namespace.to_string();
        let bucket = config.s3_bucket.clone();
        let granularity_target = context
            .output_layout
            .time_partition_granularity
            .as_deref()
            .unwrap_or("");

        let path = config.s3_prefix.clone();
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

        let is_deadletter = binding == RuntimeBinding::Deadletter
            && namespace == skippr_runtime_sdk::sink_compat::deadletter::table_name();
        if !is_deadletter {
            if let Some(contract) = source_contract {
                append_contract_glue_partition_keys(&mut partitions, contract, metadata);
            }
            AwsAthena::get_partition_by_fields(&context.output_layout, &mut partitions);
        }

        // Deadletters are always written flat to a dedicated sink, so do not
        // attach the pipeline's time partitioning config to their Glue table.
        if !is_deadletter && !granularity_target.is_empty() {
            for granularity in AwsAthena::time_partition_names_for_layout(&context.output_layout) {
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

        let columns =
            SkipprHive::storage_columns_excluding_partition_keys(metadata, &partitions).unwrap();

        let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .load()
            .await;

        let glue_client = GlueClient::new(&aws_config);

        let mut table_input = TableInput::builder()
            .name(&table_name)
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
                            .name(format!("{}.{}", &database, table_name))
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
        config: &DataSinkAthenaPluginConfig,
        namespace: &str,
        metadata: &OutputMetadata,
        existing_table: GetTableOutput,
    ) -> Result<bool, String> {
        let database = config.glue_database_name.clone();
        let table_name = namespace.to_string();
        let bucket = config.s3_bucket.clone();

        let path = config.s3_prefix.clone();
        let path = path.trim_matches('/');
        let path = std::path::Path::new(&bucket)
            .join(&path)
            .join(&namespace)
            .to_str()
            .unwrap()
            .to_string();

        let existing_partition_keys = existing_table
            .table()
            .and_then(|table| table.partition_keys.clone())
            .unwrap_or_default();
        let columns = SkipprHive::storage_columns_excluding_partition_keys(
            metadata,
            &existing_partition_keys,
        )
        .unwrap();

        let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .load()
            .await;

        let glue_client = GlueClient::new(&aws_config);

        let mut table_input = TableInput::builder()
            .name(&table_name)
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
                            .name(format!("{}.{}", &database, table_name))
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
        context: &RuntimeExecutionContext,
        binding: RuntimeBinding,
        config: &DataSinkAthenaPluginConfig,
        namespace: &str,
        partition_values: Vec<String>,
        key: &str,
        metadata: &OutputMetadata,
        source_contract: Option<&SourceNamespaceContract>,
    ) -> Result<bool, String> {
        let database = config.glue_database_name.clone();
        let table_name = namespace.to_string();
        let bucket = config.s3_bucket.clone();

        let path = std::path::Path::new(&bucket)
            .join(&key)
            .to_str()
            .unwrap()
            .to_string();

        let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .load()
            .await;

        let glue_client = GlueClient::new(&aws_config);

        // Gate by global semaphore and per-namespace mutex
        let _cp_permit = GLUE_CP_SEM.acquire().await.unwrap();
        let ns_lock = get_namespace_lock(namespace);
        let _ns_guard = ns_lock.lock().await;

        // Ensure table exists; if missing, create DB/table before proceeding.
        let existing_partition_keys = match glue_client
            .get_table()
            .database_name(&database)
            .name(&table_name)
            .send()
            .await
        {
            Ok(output) => {
                let existing_partition_keys = output
                    .table()
                    .and_then(|table| table.partition_keys.clone())
                    .unwrap_or_default();
                if existing_partition_keys.len() != partition_values.len() {
                    let key_names = existing_partition_keys
                        .iter()
                        .map(|column| column.name.clone())
                        .collect::<Vec<_>>();
                    return Err(format!(
                        "Glue partition mismatch for '{}.{}': table has {} partition keys {:?}, but Skippr generated {} partition values {:?} for key '{}'. Check the pipeline batch_partition_fields/time partition config or reset/recreate the external Glue table/schema sink state.",
                        database,
                        namespace,
                        existing_partition_keys.len(),
                        key_names,
                        partition_values.len(),
                        partition_values,
                        key
                    ));
                }
                existing_partition_keys
            }
            Err(SdkError::ServiceError(err))
                if matches!(err.err(), GetTableError::EntityNotFoundException(_)) =>
            {
                info!(
                    "Glue table '{}' not found in database '{}'; creating it before partition sync",
                    namespace, database
                );
                if !matches!(AwsAthena::glue_get_database(config).await, Ok(true)) {
                    AwsAthena::backoff_retry(
                        || AwsAthena::glue_create_database(config),
                        "create_database",
                    )
                    .await
                    .map_err(|err| {
                        format!("failed to create Glue database '{}': {}", database, err)
                    })?;
                }
                AwsAthena::backoff_retry(
                    || {
                        AwsAthena::glue_create_table(
                            context,
                            binding,
                            config,
                            namespace,
                            metadata,
                            source_contract,
                        )
                    },
                    "create_table",
                )
                .await
                .map_err(|err| {
                    format!(
                        "failed to create Glue table '{}.{}': {}",
                        database, namespace, err
                    )
                })?;
                match glue_client
                    .get_table()
                    .database_name(&database)
                    .name(&table_name)
                    .send()
                    .await
                {
                    Ok(output) => output
                        .table()
                        .and_then(|table| table.partition_keys.clone())
                        .unwrap_or_default(),
                    Err(err) => {
                        return Err(format!(
                            "failed to read Glue table '{}.{}' after create: {}",
                            database, table_name, err
                        ));
                    }
                }
            }
            Err(err) => {
                return Err(format!(
                    "failed to read Glue table '{}.{}' before partition sync: {}",
                    database, table_name, err
                ));
            }
        };

        let columns = SkipprHive::storage_columns_excluding_partition_keys(
            metadata,
            &existing_partition_keys,
        )
        .unwrap();

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
                            .name(format!("{}.{}", &database, table_name))
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

        match glue_client
            .get_partition()
            .database_name(&database)
            .table_name(&table_name)
            .set_partition_values(Some(partition_values.clone()))
            .send()
            .await
        {
            Ok(_) => {
                AwsAthena::backoff_retry(
                    || async {
                        glue_client
                            .update_partition()
                            .database_name(database.clone())
                            .table_name(&table_name)
                            .partition_input(partition_conf.clone())
                            .set_partition_value_list(Some(partition_values.clone()))
                            .send()
                            .await
                            .map(|_| true)
                            .map_err(|e| e.into_service_error().to_string())
                    },
                    "update_partition",
                )
                .await
                .map(|_| ())?;
                info!(
                    "Glue partition updated: {}.{} values={:?} location=s3://{}/",
                    database, table_name, partition_values, path
                );
            }
            Err(_) => {
                AwsAthena::backoff_retry(
                    || async {
                        match glue_client
                            .create_partition()
                            .database_name(&database)
                            .table_name(&table_name)
                            .partition_input(partition_conf.clone())
                            .send()
                            .await
                        {
                            Ok(_) => Ok(true),
                            Err(e) => {
                                let s = e.into_service_error().to_string();
                                if s.contains("AlreadyExistsException") {
                                    Ok(true)
                                } else {
                                    Err(s)
                                }
                            }
                        }
                    },
                    "create_partition",
                )
                .await
                .map(|_| ())?;
                info!(
                    "Glue partition created: {}.{} values={:?} location=s3://{}/",
                    database, table_name, partition_values, path
                );
            }
        }

        Ok(true)
    }
}

impl DataSinkAthenaPlugin {
    async fn peek_first_batch(
        mut stream: SendableRecordBatchStream,
    ) -> Result<(Option<RecordBatch>, SendableRecordBatchStream), io::Error> {
        let schema = stream.schema();
        match stream.next().await {
            Some(Ok(batch)) => {
                let first = batch.clone();
                let batches = vec![batch];
                let replay = ChainedRecordBatchStream::new(schema, batches, stream);
                Ok((Some(first), Box::pin(replay) as SendableRecordBatchStream))
            }
            Some(Err(err)) => Err(io::Error::other(err.to_string())),
            None => Ok((
                None,
                Box::pin(EmptyRecordBatchStream::new(schema)) as SendableRecordBatchStream,
            )),
        }
    }
}

struct ChainedRecordBatchStream {
    schema: Arc<arrow::datatypes::Schema>,
    prefix: Vec<RecordBatch>,
    tail: SendableRecordBatchStream,
}

impl ChainedRecordBatchStream {
    fn new(
        schema: Arc<arrow::datatypes::Schema>,
        prefix: Vec<RecordBatch>,
        tail: SendableRecordBatchStream,
    ) -> Self {
        Self {
            schema,
            prefix,
            tail,
        }
    }
}

impl futures::Stream for ChainedRecordBatchStream {
    type Item = Result<RecordBatch, datafusion::error::DataFusionError>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> TaskPoll<Option<Self::Item>> {
        if !self.prefix.is_empty() {
            return TaskPoll::Ready(Some(Ok(self.prefix.remove(0))));
        }
        Pin::new(&mut self.tail).poll_next(cx)
    }
}

impl RecordBatchStream for ChainedRecordBatchStream {
    fn schema(&self) -> Arc<arrow::datatypes::Schema> {
        self.schema.clone()
    }
}

struct EmptyRecordBatchStream {
    schema: Arc<arrow::datatypes::Schema>,
}

impl EmptyRecordBatchStream {
    fn new(schema: Arc<arrow::datatypes::Schema>) -> Self {
        Self { schema }
    }
}

impl futures::Stream for EmptyRecordBatchStream {
    type Item = Result<RecordBatch, datafusion::error::DataFusionError>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> TaskPoll<Option<Self::Item>> {
        TaskPoll::Ready(None)
    }
}

impl RecordBatchStream for EmptyRecordBatchStream {
    fn schema(&self) -> Arc<arrow::datatypes::Schema> {
        self.schema.clone()
    }
}

fn hive_type_for_skippr_data_type(dtype: &SkipprDataType) -> &'static str {
    match dtype {
        SkipprDataType::Boolean => "boolean",
        SkipprDataType::Integer => "int",
        SkipprDataType::Short => "smallint",
        SkipprDataType::Byte => "tinyint",
        SkipprDataType::Long => "bigint",
        SkipprDataType::Float => "float",
        SkipprDataType::Double => "double",
        SkipprDataType::Decimal => "decimal(38,9)",
        SkipprDataType::Date | SkipprDataType::Timestamp | SkipprDataType::TimestampMilli => {
            "timestamp"
        }
        SkipprDataType::Binary | SkipprDataType::Fixed => "binary",
        _ => "string",
    }
}

fn hive_partition_type_for_column(metadata: &OutputMetadata, column: &str) -> String {
    let segments: Vec<&str> = column.split('.').collect();
    let mut current = metadata;
    for (index, segment) in segments.iter().enumerate() {
        let child = current
            .child_fields()
            .find(|(_, field)| field.out_field_name() == *segment)
            .map(|(_, field)| field);
        let Some(child) = child else {
            return "string".to_string();
        };
        if index + 1 == segments.len() {
            return hive_type_for_skippr_data_type(child.determined_type()).to_string();
        }
        current = child;
    }
    "string".to_string()
}

fn append_contract_glue_partition_keys(
    partitions: &mut Vec<Column>,
    contract: &SourceNamespaceContract,
    metadata: &OutputMetadata,
) {
    if contract.partition_key.is_empty() {
        return;
    }
    let existing: std::collections::HashSet<String> = partitions
        .iter()
        .map(|column| column.name().to_string())
        .collect();
    for path in &contract.partition_key {
        let name = path.dotted();
        if existing.contains(&name) {
            continue;
        }
        let hive_type = hive_partition_type_for_column(metadata, &name);
        partitions.push(
            Column::builder()
                .name(name)
                .r#type(hive_type)
                .build()
                .unwrap(),
        );
    }
}

fn contract_partition_key_values(
    contract: &SourceNamespaceContract,
    batch: &RecordBatch,
) -> Result<Vec<(String, String)>, io::Error> {
    if contract.partition_key.is_empty() {
        return Ok(Vec::new());
    }
    if batch.num_rows() == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ReplacePartition requires at least one row to derive contract partition values",
        ));
    }
    let mut values = Vec::with_capacity(contract.partition_key.len());
    for path in &contract.partition_key {
        let column = path.dotted();
        let idx = batch
            .schema()
            .index_of(&column)
            .map_err(|err| io::Error::other(err.to_string()))?;
        let array = batch.column(idx);
        if array.is_null(0) {
            return Err(io::Error::other(format!(
                "partition key '{}' is null in first batch row",
                column
            )));
        }
        let value = array_value_to_string(array.as_ref(), 0)
            .map_err(|err| io::Error::other(err.to_string()))?;
        values.push((column, value));
    }
    Ok(values)
}

fn contract_partition_delete_prefix(
    namespace: &str,
    trimmed_key: &str,
    contract: &SourceNamespaceContract,
    batch: &RecordBatch,
) -> Result<String, io::Error> {
    if contract.partition_key.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "ReplacePartition for namespace '{}' requires partition_key in source contract",
                namespace
            ),
        ));
    }
    let segments: Vec<String> = contract_partition_key_values(contract, batch)?
        .into_iter()
        .map(|(column, value)| format!("{}={}", column, value))
        .collect();
    let partition_path = segments.join("/");
    let base = if trimmed_key.is_empty() {
        namespace.to_string()
    } else {
        format!("{}/{}", trimmed_key, namespace)
    };
    Ok(format!("{}/{}", base, partition_path))
}

#[cfg(test)]
mod contract_schema_tests {
    use super::*;
    use arrow::array::StringArray;
    use arrow::datatypes::{DataType, Field, Schema};
    use aws_sdk_glue::types::Column;
    use skippr_runtime_sdk::plugins::source_contract::{FieldPath, WritePolicy};

    fn ga4_replace_partition_contract() -> SourceNamespaceContract {
        SourceNamespaceContract {
            namespace: "google_analytics.events_daily".to_string(),
            primary_key: vec![FieldPath::single("property_id"), FieldPath::single("date")],
            cursor: Some(FieldPath::single("date")),
            partition_key: vec![FieldPath::single("date")],
            write_policy: WritePolicy::ReplacePartition,
            refresh_window: Some(3),
            description: String::new(),
            semantics: None,
        }
    }

    fn batch_with_date(date: &str) -> RecordBatch {
        let schema = Arc::new(Schema::new(vec![Field::new("date", DataType::Utf8, false)]));
        let dates = StringArray::from(vec![date]);
        RecordBatch::try_new(schema, vec![Arc::new(dates)]).unwrap()
    }

    #[test]
    fn contract_partition_key_becomes_glue_partition_column() {
        let contract = ga4_replace_partition_contract();
        let metadata = OutputMetadata::new();
        let mut partitions = Vec::new();
        append_contract_glue_partition_keys(&mut partitions, &contract, &metadata);
        assert_eq!(partitions.len(), 1);
        assert_eq!(partitions[0].name(), "date");
        assert_eq!(partitions[0].r#type(), Some("string"));
    }

    #[test]
    fn contract_glue_partition_keys_skip_duplicates() {
        let contract = ga4_replace_partition_contract();
        let metadata = OutputMetadata::new();
        let mut partitions = vec![Column::builder()
            .name("date")
            .r#type("string")
            .build()
            .unwrap()];
        append_contract_glue_partition_keys(&mut partitions, &contract, &metadata);
        assert_eq!(partitions.len(), 1);
    }

    #[test]
    fn contract_partition_key_values_from_batch() {
        let contract = ga4_replace_partition_contract();
        let batch = batch_with_date("2024-01-15");
        let values = contract_partition_key_values(&contract, &batch).unwrap();
        assert_eq!(values, vec![("date".to_string(), "2024-01-15".to_string())]);
    }

    #[test]
    fn contract_partition_delete_prefix_matches_s3_layout() {
        let contract = ga4_replace_partition_contract();
        let batch = batch_with_date("2024-01-15");
        let prefix = contract_partition_delete_prefix(
            "google_analytics.events_daily",
            "bronze",
            &contract,
            &batch,
        )
        .unwrap();
        assert_eq!(
            prefix,
            "bronze/google_analytics.events_daily/date=2024-01-15"
        );
    }

    #[test]
    fn contract_partition_empty_batch_is_rejected() {
        let contract = ga4_replace_partition_contract();
        let schema = Arc::new(Schema::new(vec![Field::new("date", DataType::Utf8, false)]));
        let batch = RecordBatch::new_empty(schema);
        let err = contract_partition_key_values(&contract, &batch).unwrap_err();
        assert!(err.to_string().contains("at least one row"));
    }

    #[test]
    fn contract_partition_null_key_is_rejected() {
        let contract = ga4_replace_partition_contract();
        let schema = Arc::new(Schema::new(vec![Field::new("date", DataType::Utf8, true)]));
        let dates = StringArray::from(vec![None as Option<&str>]);
        let batch = RecordBatch::try_new(schema, vec![Arc::new(dates)]).unwrap();
        let err = contract_partition_key_values(&contract, &batch).unwrap_err();
        assert!(err.to_string().contains("null"));
    }

    #[test]
    fn contract_partition_missing_column_is_rejected() {
        let contract = ga4_replace_partition_contract();
        let schema = Arc::new(Schema::new(vec![Field::new(
            "other",
            DataType::Utf8,
            false,
        )]));
        let values = StringArray::from(vec!["x"]);
        let batch = RecordBatch::try_new(schema, vec![Arc::new(values)]).unwrap();
        assert!(contract_partition_key_values(&contract, &batch).is_err());
    }

    #[test]
    fn contract_partition_delete_prefix_requires_partition_key() {
        let contract = SourceNamespaceContract {
            namespace: "ns".into(),
            primary_key: vec![],
            cursor: None,
            partition_key: vec![],
            write_policy: WritePolicy::ReplacePartition,
            refresh_window: None,
            description: String::new(),
            semantics: None,
        };
        let batch = batch_with_date("2024-01-01");
        let err = contract_partition_delete_prefix("ns", "", &contract, &batch).unwrap_err();
        assert!(err.to_string().contains("partition_key"));
    }
}
