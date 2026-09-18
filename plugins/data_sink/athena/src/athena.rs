use crate::helpers::configuration::DataSinkPluginConfig;
use crate::helpers::Helpers;
use aws_sdk_athena::types::{
    EncryptionConfiguration, EncryptionOption, ResultConfiguration, ResultConfigurationUpdates,
    Tag, WorkGroupConfiguration, WorkGroupConfigurationUpdates,
};
use aws_sdk_athena::Client as AthenaClient;
use aws_sdk_glue::types::{
    Column, DatabaseInput, PartitionIndex, SerDeInfo, StorageDescriptor, TableInput,
};
use aws_sdk_glue::Client as GlueClient;
use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError as S3SdkError};
use aws_sdk_s3::operation::get_object::GetObjectError;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use aws_sdk_s3::types::{Delete, ObjectIdentifier};
use aws_sdk_s3::Client as S3Client;
use skippr_object_writer::{
    CompletionMetadata, MultipartUpload, ObjectPartReceipt, ObjectWriteBackend, ObjectWriteError,
    ObjectWriteRequest, ObjectWriteSession, ObjectWriterConfig, PartMetadata,
};
use skippr_runtime_sdk::converters::skippr_hive::SkipprHive;
use skippr_runtime_sdk::discover::{OutputMetadata, SkipprDataType};
use skippr_runtime_sdk::metrics::counters as metrics_counters;
use skippr_runtime_sdk::plugins::{SchemaSink, SchemaSyncRequest, SinkWriteOutcome};
use skippr_runtime_sdk::sink_compat::BufferChunker;
use skippr_runtime_sdk::sink_idempotency::{
    legacy_chunk_idempotency_key, sidecar_manifest_object_key, GroupedWriteReceipt,
    ObjectWriteManifest,
};
use skippr_runtime_sdk::SkipprConfig;

use arrow::array::RecordBatch;
use arrow::util::display::array_value_to_string;
use async_trait::async_trait;
use aws_sdk_glue::error::SdkError;
use aws_sdk_glue::operation::get_table::{GetTableError, GetTableOutput};
use bytes::Bytes;
use datafusion::physical_plan::RecordBatchStream;
use datafusion::physical_plan::SendableRecordBatchStream;
use futures::StreamExt;
use std::collections::{BTreeMap, HashMap};
use std::io;
use std::num::NonZeroU64;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll as TaskPoll};

use dashmap::DashMap;
use once_cell::sync::Lazy;
use rand::Rng;
use serde_derive::Deserialize;
use skippr_runtime_sdk::plugins::source_contract::{
    ensure_source_contract_for_policy, namespace_source_contract, validate_write_policy_for_sink,
    SinkWritePolicySupport, SourceNamespaceContract, WritePolicy,
};
use skippr_runtime_sdk::plugins::{
    DataSink, SinkCallResult, SinkPreflightOutcome, SinkPreflightResult, SinkWriteContext,
};
use skippr_runtime_sdk::protocol::{
    CatalogIntent, CatalogIntentIdentity, CatalogIntentKind, GlueColumnIntent,
    GluePartitionCatalogIntentV1, RuntimeBinding, RuntimeExecutionContext, RuntimeSchemaState,
    SchemaDelta, SinkWriteStats, CATALOG_INTENT_VERSION, GLUE_PARTITION_CATALOG_INTENT_VERSION,
};
use skippr_runtime_sdk::sink_compat::partition_time::TimePartitioner;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{Mutex, RwLock, Semaphore};
use tokio::time::{sleep as tokio_sleep, Duration as TokioDuration};
use tracing::{debug, info, warn};

const TIME_PARTITION_GRANULARITIES: [&str; 5] = ["year", "month", "day", "hour", "minute"];
const ATHENA_MULTIPART_PART_SIZE: usize = 16 * 1024 * 1024;
const ATHENA_BATCH_CHANNEL_CAPACITY: usize = 1;
const ATHENA_BYTE_CHANNEL_CAPACITY: usize = 1;
const ATHENA_MULTIPART_BEGIN_TIMEOUT_SECS: u64 = 120;
const ATHENA_MULTIPART_PART_TIMEOUT_SECS: u64 = 300;
const ATHENA_MULTIPART_COMPLETE_TIMEOUT_SECS: u64 = 600;

fn athena_object_key(
    full_key: &str,
    filename: &str,
    object_stem: Option<&str>,
) -> (String, String) {
    let object_stem = object_stem
        .map(str::to_string)
        .unwrap_or_else(|| hex::encode(md5::compute(filename).0));
    let final_key = format!("{full_key}/{object_stem}.parquet");
    (object_stem, final_key)
}

fn athena_object_writer_config() -> ObjectWriterConfig {
    let max_in_flight_parts =
        skippr_runtime_sdk::metrics::counters::MULTIPART_PART_CONCURRENCY_TARGET
            .load(Ordering::Relaxed)
            .clamp(1, 8);
    ObjectWriterConfig {
        part_size: ATHENA_MULTIPART_PART_SIZE,
        batch_channel_capacity: ATHENA_BATCH_CHANNEL_CAPACITY,
        byte_channel_capacity: ATHENA_BYTE_CHANNEL_CAPACITY,
        max_in_flight_parts,
    }
}

fn sum_optional(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (None, None) => None,
        (left, right) => Some(left.unwrap_or(0).saturating_add(right.unwrap_or(0))),
    }
}

fn published_catalog_schema_version(
    namespace_versions: &BTreeMap<String, u64>,
    namespace: &str,
) -> Option<NonZeroU64> {
    namespace_versions
        .get(namespace)
        .copied()
        .and_then(NonZeroU64::new)
}

fn catalog_intent_schema_version(
    namespace_versions: &BTreeMap<String, u64>,
    namespace: &str,
) -> io::Result<NonZeroU64> {
    published_catalog_schema_version(namespace_versions, namespace).ok_or_else(|| {
        io::Error::other(format!(
            "Athena catalog intent for '{namespace}' requires an installed schema version"
        ))
    })
}

fn grouped_receipt_from_applied(
    manifest: &ObjectWriteManifest,
    bucket: &str,
    applied: &InnerSyncApplied,
    verified_etag: String,
    transport_chunk_count: u32,
) -> GroupedWriteReceipt {
    GroupedWriteReceipt::from_manifest_and_upload(
        manifest,
        format!("s3://{bucket}/{}", applied.final_key),
        verified_etag,
        applied.checksum.clone(),
        applied.rows,
        applied.bytes,
        transport_chunk_count,
    )
}

const ATHENA_WRITE_POLICY_SUPPORT: SinkWritePolicySupport = SinkWritePolicySupport {
    supports_merge_by_key: false,
    supports_replace_partition: true,
    supports_replace_table: true,
};

// Global control-plane throttling and serialization (target auto-tuned; env overrides seed)
static GLUE_CP_SEM: Lazy<Arc<Semaphore>> = Lazy::new(|| {
    let permits = seed_athena_glue_cp_target();
    Arc::new(Semaphore::new(permits.max(1)))
});
static ATHENA_WG_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));
static NAMESPACE_LOCKS: Lazy<DashMap<String, Arc<Mutex<()>>>> = Lazy::new(|| DashMap::new());

fn get_namespace_lock(namespace: &str) -> Arc<Mutex<()>> {
    NAMESPACE_LOCKS
        .entry(namespace.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// Seed this runtime process from the same measured flush-budget model.
fn seed_athena_glue_cp_target() -> usize {
    skippr_runtime_sdk::ingest::tuner::apply_env_caps();
    skippr_runtime_sdk::ingest::tuner::current_flush_budget()
        .catalog_operations
        .max(1)
}

fn resize_glue_cp_sem_to_target() {
    let target = metrics_counters::ATHENA_GLUE_CP_TARGET
        .load(Ordering::Relaxed)
        .clamp(1, 32);
    // Approximate current capacity: available + 1 if someone holds (same pattern as upload_sem).
    let available = GLUE_CP_SEM.available_permits();
    if target > available {
        GLUE_CP_SEM.add_permits(target - available);
    }
    // Shrinking is best-effort: we cannot revoke held permits; future acquires still serialize
    // via fewer effective concurrent holders when target drops (callers should check target).
}

async fn acquire_glue_cp_permit() -> tokio::sync::OwnedSemaphorePermit {
    resize_glue_cp_sem_to_target();
    // If target shrank below available, burn excess permits without blocking forever.
    let target = metrics_counters::ATHENA_GLUE_CP_TARGET
        .load(Ordering::Relaxed)
        .max(1);
    while GLUE_CP_SEM.available_permits() > target {
        if let Ok(extra) = GLUE_CP_SEM.clone().try_acquire_owned() {
            std::mem::forget(extra);
        } else {
            break;
        }
    }
    GLUE_CP_SEM
        .clone()
        .acquire_owned()
        .await
        .expect("GLUE_CP_SEM closed")
}

fn note_glue_transient_retry() {
    metrics_counters::add_glue_retry(1);
    skippr_runtime_sdk::ingest::tuner::update_flush_budget(
        skippr_runtime_sdk::ingest::tuner::FlushMode::Drain,
        true,
        None,
    );
}

fn maybe_restore_glue_cp_target() {
    skippr_runtime_sdk::ingest::tuner::update_flush_budget(
        skippr_runtime_sdk::ingest::tuner::FlushMode::Drain,
        true,
        None,
    );
}

#[derive(Debug, Deserialize, SkipprConfig, Clone, PartialEq, Eq)]
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
    /// Query/model only; ignored at ingest.
    #[serde(default)]
    #[allow(dead_code)]
    pub region: Option<String>,
    /// Query/model only; ignored at ingest.
    #[serde(default)]
    #[allow(dead_code)]
    pub catalog: Option<String>,
    /// Query/model only; ignored at ingest.
    #[serde(default)]
    #[allow(dead_code)]
    pub max_concurrency: Option<usize>,
    /// Query/model only; ignored at ingest.
    #[serde(default)]
    #[allow(dead_code)]
    pub discovery_cache_ttl_secs: Option<u64>,
}

#[derive(Clone)]
struct AthenaS3ObjectWriteBackend {
    client: S3Client,
    bucket: String,
    object_key: String,
    tagging: String,
}

impl AthenaS3ObjectWriteBackend {
    fn new(client: S3Client, bucket: String, object_key: String, tagging: String) -> Self {
        Self {
            client,
            bucket,
            object_key,
            tagging,
        }
    }

    fn validate_request_key(&self, request: &ObjectWriteRequest) -> io::Result<()> {
        if request.object_key == self.object_key {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Athena S3 backend configured for key '{}' but received '{}'",
                    self.object_key, request.object_key
                ),
            ))
        }
    }
}

#[async_trait]
impl ObjectWriteBackend for AthenaS3ObjectWriteBackend {
    type Error = io::Error;

    async fn begin(&self, request: &ObjectWriteRequest) -> Result<MultipartUpload, Self::Error> {
        self.validate_request_key(request)?;
        let metadata = request
            .metadata
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<HashMap<_, _>>();
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(ATHENA_MULTIPART_BEGIN_TIMEOUT_SECS),
            self.client
                .create_multipart_upload()
                .bucket(&self.bucket)
                .key(&request.object_key)
                .content_type(&request.content_type)
                .tagging(&self.tagging)
                .set_metadata((!metadata.is_empty()).then_some(metadata))
                .send(),
        )
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "Timeout initiating multipart upload",
            )
        })?
        .map_err(|error| {
            io::Error::other(format!("Failed to initiate multipart upload: {error}"))
        })?;
        let upload_id = response.upload_id().unwrap_or_default().to_string();
        if upload_id.is_empty() {
            return Err(io::Error::other("Missing upload_id from S3"));
        }
        Ok(MultipartUpload {
            upload_id,
            metadata: BTreeMap::new(),
        })
    }

    async fn upload_part(
        &self,
        upload: &MultipartUpload,
        part_number: u32,
        bytes: Bytes,
    ) -> Result<PartMetadata, Self::Error> {
        let part_number = i32::try_from(part_number)
            .map_err(|_| io::Error::other("S3 multipart part number exceeded i32::MAX"))?;
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(ATHENA_MULTIPART_PART_TIMEOUT_SECS),
            self.client
                .upload_part()
                .bucket(&self.bucket)
                .key(&self.object_key)
                .upload_id(&upload.upload_id)
                .part_number(part_number)
                .body(ByteStream::from(bytes))
                .send(),
        )
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "upload_part timed out"))?
        .map_err(|error| io::Error::other(format!("upload_part failed: {error}")))?;
        let etag = response.e_tag().unwrap_or_default().to_string();
        if etag.is_empty() {
            return Err(io::Error::other(format!(
                "upload_part returned no ETag for part {part_number}"
            )));
        }
        Ok(PartMetadata {
            etag: Some(etag),
            checksum: None,
            metadata: BTreeMap::new(),
        })
    }

    async fn complete(
        &self,
        upload: &MultipartUpload,
        parts: &[ObjectPartReceipt],
    ) -> Result<CompletionMetadata, Self::Error> {
        let completed_parts = parts
            .iter()
            .map(|part| {
                let part_number = i32::try_from(part.part_number)
                    .map_err(|_| io::Error::other("S3 multipart part number exceeded i32::MAX"))?;
                let etag = part.etag.clone().ok_or_else(|| {
                    io::Error::other(format!(
                        "S3 multipart part {} has no ETag",
                        part.part_number
                    ))
                })?;
                Ok(CompletedPart::builder()
                    .part_number(part_number)
                    .e_tag(etag)
                    .build())
            })
            .collect::<io::Result<Vec<_>>>()?;
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(ATHENA_MULTIPART_COMPLETE_TIMEOUT_SECS),
            self.client
                .complete_multipart_upload()
                .bucket(&self.bucket)
                .key(&self.object_key)
                .upload_id(&upload.upload_id)
                .multipart_upload(
                    CompletedMultipartUpload::builder()
                        .set_parts(Some(completed_parts))
                        .build(),
                )
                .send(),
        )
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "complete_multipart_upload timed out",
            )
        })?
        .map_err(|error| io::Error::other(format!("complete_multipart_upload failed: {error}")))?;
        Ok(CompletionMetadata {
            etag: response.e_tag().map(str::to_string),
            checksum: None,
            version_id: response.version_id().map(str::to_string),
            metadata: BTreeMap::new(),
        })
    }

    async fn abort(&self, upload: &MultipartUpload) -> Result<(), Self::Error> {
        self.client
            .abort_multipart_upload()
            .bucket(&self.bucket)
            .key(&self.object_key)
            .upload_id(&upload.upload_id)
            .send()
            .await
            .map_err(|error| io::Error::other(format!("abort_multipart_upload failed: {error}")))?;
        Ok(())
    }
}

struct UploadInFlightGuard;

impl UploadInFlightGuard {
    fn start() -> Self {
        metrics_counters::inc_uploads_in_flight();
        Self
    }
}

impl Drop for UploadInFlightGuard {
    fn drop(&mut self) {
        metrics_counters::dec_uploads_in_flight();
    }
}

impl TryFrom<DataSinkPluginConfig> for DataSinkAthenaPluginConfig {
    type Error = String;

    fn try_from(entry: DataSinkPluginConfig) -> Result<Self, Self::Error> {
        entry.decode_for_plugin("Athena")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InnerSyncApplied {
    pub final_key: String,
    pub rows: u64,
    pub bytes: u64,
    pub etag: Option<String>,
    pub checksum: Option<String>,
    pub catalog_intents: Vec<CatalogIntent>,
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
    region: Option<String>,
    catalog_id: Option<String>,
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
    installed: bool,
    version: u64,
    namespaces: BTreeMap<String, OutputMetadata>,
    namespace_versions: BTreeMap<String, u64>,
}

impl InstalledAthenaSchemaState {
    fn install(&mut self, schema_version: u64, namespaces: &BTreeMap<String, OutputMetadata>) {
        if self.installed && schema_version <= self.version {
            return;
        }
        self.installed = true;
        self.version = schema_version;
        self.namespaces = namespaces.clone();
        self.namespace_versions = namespaces
            .keys()
            .map(|namespace| (namespace.clone(), schema_version))
            .collect();
    }

    fn install_snapshot(&mut self, schema_state: &RuntimeSchemaState) {
        if self.installed && schema_state.version <= self.version {
            return;
        }
        self.installed = true;
        self.version = schema_state.version;
        self.namespaces = schema_state.namespaces.clone();
        self.namespace_versions = schema_state.namespace_versions.clone();
    }

    fn install_delta(&mut self, delta: &SchemaDelta) -> usize {
        let mut changed = 0;
        for (namespace, entry) in &delta.namespaces {
            if self
                .namespace_versions
                .get(namespace)
                .is_some_and(|installed| *installed >= entry.version)
            {
                continue;
            }
            self.namespaces
                .insert(namespace.clone(), entry.metadata.clone());
            self.namespace_versions
                .insert(namespace.clone(), entry.version);
            changed += 1;
        }
        self.installed = true;
        self.version = self.version.max(delta.version);
        changed
    }
}

fn is_deadletter_athena_target(binding: RuntimeBinding, namespace: &str) -> bool {
    binding == RuntimeBinding::Deadletter
        && namespace
            == skippr_runtime_sdk::sink_compat::deadletter::table_name(
                &skippr_runtime_sdk::helpers::configuration::Config::new(),
            )
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
    async fn preflight(
        &self,
        ctx: SinkWriteContext<'_>,
    ) -> Result<SinkPreflightOutcome, std::io::Error> {
        Ok(self.preflight_result(ctx).await?.outcome)
    }

    async fn preflight_result(
        &self,
        ctx: SinkWriteContext<'_>,
    ) -> Result<SinkPreflightResult, std::io::Error> {
        if !ctx.is_grouped() {
            return Ok(SinkPreflightResult {
                outcome: SinkPreflightOutcome::Ready,
                catalog_intents: Vec::new(),
            });
        }
        ctx.validate_grouped::<skippr_runtime_sdk::plugins::DeterministicObjectOverwrite>()
            .map_err(|err| io::Error::new(io::ErrorKind::Unsupported, err.to_string()))?;
        let namespace = BufferChunker::decode_file_namespace(&ctx.filename);
        let manifest = ObjectWriteManifest::from_context(
            ctx.compaction_id.clone(),
            ctx.idempotency_key,
            ctx.schema_fingerprint,
            &ctx.wal_refs,
        );
        let receipt_key = ObjectWriteManifest::grouped_receipt_object_key(
            &self.config.s3_prefix,
            &namespace,
            &ctx.compaction_id,
        );
        if let Some(existing) = self.read_grouped_receipt(&receipt_key).await? {
            if !existing.matches_manifest(&manifest) {
                return Err(io::Error::other(format!(
                    "grouped receipt mismatch for compaction {}",
                    ctx.compaction_id
                )));
            }
            let catalog_intents =
                match partition_values_from_object_key(&namespace, &existing.final_s3_key) {
                    Some((full_key, partition_values)) => {
                        let metadata = self.namespace_metadata(&namespace).await?;
                        self.partition_catalog_intent(
                            &namespace,
                            partition_values,
                            &full_key,
                            &metadata,
                            ctx.source_contract,
                        )
                        .await?
                        .into_iter()
                        .collect()
                    }
                    None => Vec::new(),
                };
            return Ok(SinkPreflightResult {
                outcome: SinkPreflightOutcome::AlreadyApplied {
                    authority: format!("s3://{}/{}", self.config.s3_bucket, receipt_key),
                },
                catalog_intents,
            });
        }
        Ok(SinkPreflightResult {
            outcome: SinkPreflightOutcome::Ready,
            catalog_intents: Vec::new(),
        })
    }

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
            None,
        )
        .await
        .map(|_| ())
    }

    async fn sync_with_context_result(
        &self,
        stream: SendableRecordBatchStream,
        ctx: SinkWriteContext<'_>,
    ) -> Result<SinkWriteOutcome, std::io::Error> {
        self.sync_with_context_call_result(stream, ctx)
            .await
            .map(|result| result.outcome)
    }

    async fn sync_with_context_call_result(
        &self,
        stream: SendableRecordBatchStream,
        ctx: SinkWriteContext<'_>,
    ) -> Result<SinkCallResult, std::io::Error> {
        if !ctx.is_grouped() {
            self.sync_with_context(stream, ctx).await?;
            return Ok(SinkCallResult::outcome(SinkWriteOutcome::Applied));
        }
        ctx.validate_grouped::<skippr_runtime_sdk::plugins::DeterministicObjectOverwrite>()
            .map_err(|err| io::Error::new(io::ErrorKind::Unsupported, err.to_string()))?;
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
        let manifest = ObjectWriteManifest::from_context(
            ctx.compaction_id.clone(),
            ctx.idempotency_key.clone(),
            ctx.schema_fingerprint.clone(),
            &ctx.wal_refs,
        );
        let (outcome, applied) = self
            .inner_sync(
                stream,
                ctx.filename,
                write_policy,
                resolved_contract.as_ref(),
                Some(ctx.idempotency_key.as_str()),
                Some(&manifest),
            )
            .await?;
        let mut result = SinkCallResult::outcome(outcome);
        if let Some(applied) = applied {
            result.stats = SinkWriteStats {
                rows: (applied.rows > 0).then_some(applied.rows),
                bytes: (applied.bytes > 0).then_some(applied.bytes),
                objects: Some(1),
                ..SinkWriteStats::default()
            };
            result.catalog_intents = applied.catalog_intents;
        }
        Ok(result)
    }

    async fn sync_grouped(
        &self,
        mut reader: skippr_runtime_sdk::plugins::GroupedBatchReader,
        ctx: skippr_runtime_sdk::plugins::GroupedSinkWriteContext<'_>,
    ) -> Result<SinkWriteOutcome, std::io::Error> {
        ctx.to_sink_write_context()
            .validate_grouped::<skippr_runtime_sdk::plugins::DeterministicObjectOverwrite>()
            .map_err(|err| io::Error::new(io::ErrorKind::Unsupported, err.to_string()))?;
        if self.should_use_legacy_grouped_chunks(&ctx).await? {
            return self
                .sync_grouped_legacy_chunks(&mut reader, ctx)
                .await
                .map(|result| result.outcome);
        }
        self.sync_grouped_single_object(reader, ctx)
            .await
            .map(|result| result.outcome)
    }

    async fn sync_grouped_call_result(
        &self,
        mut reader: skippr_runtime_sdk::plugins::GroupedBatchReader,
        ctx: skippr_runtime_sdk::plugins::GroupedSinkWriteContext<'_>,
    ) -> Result<SinkCallResult, std::io::Error> {
        ctx.to_sink_write_context()
            .validate_grouped::<skippr_runtime_sdk::plugins::DeterministicObjectOverwrite>()
            .map_err(|err| io::Error::new(io::ErrorKind::Unsupported, err.to_string()))?;
        if self.should_use_legacy_grouped_chunks(&ctx).await? {
            return self.sync_grouped_legacy_chunks(&mut reader, ctx).await;
        }
        self.sync_grouped_single_object(reader, ctx).await
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
        guard.install(schema_version, namespaces);
        Ok(())
    }

    async fn install_schema_snapshot(
        &self,
        schema_state: &RuntimeSchemaState,
    ) -> Result<(), std::io::Error> {
        self.schema_state
            .write()
            .await
            .install_snapshot(schema_state);
        Ok(())
    }

    async fn install_schema_delta(&self, delta: &SchemaDelta) -> Result<(), std::io::Error> {
        self.schema_state.write().await.install_delta(delta);
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
    async fn sync_grouped_legacy_chunks(
        &self,
        reader: &mut skippr_runtime_sdk::plugins::GroupedBatchReader,
        ctx: skippr_runtime_sdk::plugins::GroupedSinkWriteContext<'_>,
    ) -> Result<SinkCallResult, std::io::Error> {
        let schema = reader.schema();
        let mut applied = false;
        let mut result = SinkCallResult::outcome(SinkWriteOutcome::AlreadyApplied);
        while let Some(chunk) = reader.next_chunk().await? {
            let chunk_cdc = ctx.chunk_cdc_context(&chunk)?;
            let chunk_ctx = ctx.chunk_sink_write_context_with_cdc(
                chunk.chunk_index,
                chunk.chunk_index == 0 && chunk.final_chunk,
                chunk_cdc.as_ref(),
            );
            let chunk_result = self
                .sync_with_context_call_result(chunk.into_stream(schema.clone()), chunk_ctx)
                .await?;
            if chunk_result.outcome == SinkWriteOutcome::Applied {
                applied = true;
            }
            result.stats.rows = sum_optional(result.stats.rows, chunk_result.stats.rows);
            result.stats.bytes = sum_optional(result.stats.bytes, chunk_result.stats.bytes);
            result.stats.objects = sum_optional(result.stats.objects, chunk_result.stats.objects);
            result.catalog_intents.extend(chunk_result.catalog_intents);
        }
        result.outcome = if applied {
            SinkWriteOutcome::Applied
        } else {
            SinkWriteOutcome::AlreadyApplied
        };
        Ok(result)
    }

    async fn sync_grouped_single_object(
        &self,
        reader: skippr_runtime_sdk::plugins::GroupedBatchReader,
        ctx: skippr_runtime_sdk::plugins::GroupedSinkWriteContext<'_>,
    ) -> Result<SinkCallResult, std::io::Error> {
        let namespace = BufferChunker::decode_file_namespace(&ctx.filename);
        let resolved_contract = ctx
            .source_contract
            .cloned()
            .or_else(|| namespace_source_contract(&namespace));
        let write_policy = resolved_contract
            .as_ref()
            .map(|c| c.write_policy)
            .unwrap_or(WritePolicy::Append);
        let manifest = ObjectWriteManifest::from_context(
            ctx.compaction_id.clone(),
            ctx.idempotency_key.clone(),
            ctx.schema_fingerprint.clone(),
            &ctx.wal_refs.clone_vec(),
        );
        let receipt_key = ObjectWriteManifest::grouped_receipt_object_key(
            &self.config.s3_prefix,
            &namespace,
            &ctx.compaction_id,
        );
        if let Some(existing) = self.read_grouped_receipt(&receipt_key).await? {
            if existing.matches_manifest(&manifest) {
                let catalog_intents =
                    match partition_values_from_object_key(&namespace, &existing.final_s3_key) {
                        Some((full_key, partition_values)) => {
                            let metadata = self.namespace_metadata(&namespace).await?;
                            self.partition_catalog_intent(
                                &namespace,
                                partition_values,
                                &full_key,
                                &metadata,
                                ctx.source_contract,
                            )
                            .await?
                            .into_iter()
                            .collect()
                        }
                        None => Vec::new(),
                    };
                return Ok(SinkCallResult {
                    outcome: SinkWriteOutcome::AlreadyApplied,
                    stats: SinkWriteStats {
                        rows: Some(existing.rows),
                        bytes: Some(existing.bytes),
                        objects: Some(1),
                        ..SinkWriteStats::default()
                    },
                    catalog_intents,
                });
            }
            return Err(io::Error::other(format!(
                "grouped receipt mismatch for compaction {}",
                ctx.compaction_id
            )));
        }
        let (stream, stream_progress) = reader.into_stream();
        // Do not pass ObjectWriteManifest into inner_sync: it writes the sidecar to the
        // same key as GroupedWriteReceipt. Concurrent retries would then parse the
        // intermediate ObjectWriteManifest as a receipt and fail on `final_s3_key`.
        // GroupedWriteReceipt is the sole idempotency record for this path.
        let (outcome, applied) = self
            .inner_sync(
                stream,
                ctx.filename.clone(),
                write_policy,
                resolved_contract.as_ref(),
                Some(ctx.compaction_id.as_str()),
                None,
            )
            .await?;
        let mut result = SinkCallResult::outcome(outcome.clone());
        if matches!(&outcome, SinkWriteOutcome::Applied) {
            if let Some(applied) = applied {
                self.write_grouped_receipt_from_upload(
                    &manifest,
                    &namespace,
                    &applied,
                    stream_progress.transport_chunk_count(),
                )
                .await?;
                result.stats = SinkWriteStats {
                    rows: Some(applied.rows),
                    bytes: Some(applied.bytes),
                    objects: Some(1),
                    ..SinkWriteStats::default()
                };
                result.catalog_intents = applied.catalog_intents;
            }
        }
        Ok(result)
    }

    async fn should_use_legacy_grouped_chunks(
        &self,
        ctx: &skippr_runtime_sdk::plugins::GroupedSinkWriteContext<'_>,
    ) -> io::Result<bool> {
        self.legacy_chunk_manifest_exists(&ctx.compaction_id, &ctx.grouping_key.namespace)
            .await
    }

    async fn legacy_chunk_manifest_exists(
        &self,
        compaction_id: &str,
        namespace: &str,
    ) -> io::Result<bool> {
        // Legacy grouped writes were ordered and always began with chunk zero.
        // One authoritative sidecar probe preserves replay compatibility without
        // placing hundreds of sequential S3 HEAD requests on every new write.
        let chunk_key = legacy_chunk_idempotency_key(compaction_id, 0);
        let manifest_key =
            sidecar_manifest_object_key(&self.config.s3_prefix, namespace, &chunk_key);
        self.object_exists(&manifest_key).await
    }

    async fn object_exists(&self, key: &str) -> io::Result<bool> {
        match self
            .s3_client
            .head_object()
            .bucket(&self.config.s3_bucket)
            .key(key)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(err) => {
                if is_s3_not_found_error(&err) {
                    Ok(false)
                } else {
                    Err(io::Error::other(err.to_string()))
                }
            }
        }
    }

    async fn read_grouped_receipt(
        &self,
        receipt_key: &str,
    ) -> io::Result<Option<GroupedWriteReceipt>> {
        let response = match self
            .s3_client
            .get_object()
            .bucket(&self.config.s3_bucket)
            .key(receipt_key)
            .send()
            .await
        {
            Ok(response) => response,
            Err(err) if is_s3_get_object_not_found_error(&err) => return Ok(None),
            Err(err) => return Err(io::Error::other(err.to_string())),
        };
        let bytes = response
            .body
            .collect()
            .await
            .map_err(|err| io::Error::other(err.to_string()))?
            .into_bytes();
        match GroupedWriteReceipt::from_json_bytes(&bytes) {
            Ok(receipt) => Ok(Some(receipt)),
            Err(err) => {
                // Intermediate ObjectWriteManifest (or other non-receipt JSON) at the
                // receipt key means a prior attempt did not finish writing the receipt —
                // treat as absent so the caller can re-apply.
                if ObjectWriteManifest::from_json_bytes(&bytes).is_ok() {
                    warn!(
                        "Ignoring incomplete idempotency object at {} (not a grouped receipt yet)",
                        receipt_key
                    );
                    return Ok(None);
                }
                Err(err)
            }
        }
    }

    async fn write_grouped_receipt_from_upload(
        &self,
        manifest: &ObjectWriteManifest,
        namespace: &str,
        applied: &InnerSyncApplied,
        transport_chunk_count: u32,
    ) -> io::Result<()> {
        let receipt_key = ObjectWriteManifest::grouped_receipt_object_key(
            &self.config.s3_prefix,
            namespace,
            &manifest.compaction_id,
        );
        let head = self
            .s3_client
            .head_object()
            .bucket(&self.config.s3_bucket)
            .key(&applied.final_key)
            .send()
            .await
            .map_err(|err| io::Error::other(err.to_string()))?;
        let etag = head
            .e_tag()
            .filter(|etag| !etag.is_empty())
            .ok_or_else(|| {
                io::Error::other(format!(
                    "S3 HEAD returned no ETag for grouped object {}",
                    applied.final_key
                ))
            })?
            .to_string();
        if let Some(completed_etag) = applied.etag.as_deref() {
            if completed_etag != etag {
                return Err(io::Error::other(format!(
                    "S3 ETag mismatch for grouped object {}: complete returned {}, HEAD returned {}",
                    applied.final_key, completed_etag, etag
                )));
            }
        }
        let receipt = grouped_receipt_from_applied(
            manifest,
            &self.config.s3_bucket,
            applied,
            etag,
            transport_chunk_count,
        );
        self.s3_client
            .put_object()
            .bucket(&self.config.s3_bucket)
            .key(&receipt_key)
            .body(ByteStream::from(receipt.to_json_bytes()?))
            .send()
            .await
            .map_err(|err| io::Error::other(err.to_string()))?;
        Ok(())
    }

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
        let region = aws_config
            .region()
            .map(|region| region.as_ref().to_string());
        let catalog_id = std::env::var_os("AWS_GLUE_CATALOG_ID")
            .and_then(|value| value.into_string().ok())
            .filter(|value| !value.trim().is_empty());
        let _ = seed_athena_glue_cp_target();
        let tuned_uploads = skippr_runtime_sdk::metrics::counters::UPLOAD_CONCURRENCY_TARGET
            .load(std::sync::atomic::Ordering::Relaxed);
        let max_async_uploads = tuned_uploads.max(1);

        Self {
            s3_client,
            athena_client,
            context,
            binding,
            config: athena_config,
            region,
            catalog_id,
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

    async fn partition_catalog_intent(
        &self,
        namespace: &str,
        partition_values: Vec<String>,
        full_key: &str,
        metadata: &OutputMetadata,
        source_contract: Option<&SourceNamespaceContract>,
    ) -> io::Result<Option<CatalogIntent>> {
        if partition_values.is_empty() {
            return Ok(None);
        }
        let partition_columns = build_glue_partition_keys(
            &self.context,
            self.binding,
            namespace,
            source_contract,
            metadata,
            None,
        );
        if partition_columns.len() != partition_values.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Athena catalog intent for '{}' has {} values but {} partition columns",
                    namespace,
                    partition_values.len(),
                    partition_columns.len()
                ),
            ));
        }
        let storage_columns =
            SkipprHive::storage_columns_excluding_partition_keys(metadata, &partition_columns)
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("failed to derive Glue storage columns for '{namespace}'"),
                    )
                })?;
        let schema_version = catalog_intent_schema_version(
            &self.schema_state.read().await.namespace_versions,
            namespace,
        )?;
        let column_intent = |column: &Column| {
            GlueColumnIntent::from_glue_fields(column.name(), column.r#type(), column.comment())
        };
        let location = format!(
            "s3://{}/{}/",
            self.config.s3_bucket,
            full_key.trim_matches('/')
        );
        let payload = GluePartitionCatalogIntentV1 {
            version: GLUE_PARTITION_CATALOG_INTENT_VERSION,
            region: self.region.clone(),
            catalog_id: self.catalog_id.clone(),
            database: self.config.glue_database_name.clone(),
            table: namespace.to_string(),
            partition_values: partition_values.clone(),
            location,
            storage_columns: storage_columns.iter().map(column_intent).collect(),
            partition_columns: partition_columns.iter().map(column_intent).collect(),
            input_format: "org.apache.hadoop.hive.ql.io.parquet.MapredParquetInputFormat"
                .to_string(),
            output_format: "org.apache.hadoop.hive.ql.io.parquet.MapredParquetOutputFormat"
                .to_string(),
            serde_library: "org.apache.hadoop.hive.ql.io.parquet.serde.ParquetHiveSerDe"
                .to_string(),
            schema_namespace: namespace.to_string(),
            schema_version,
        };
        let key = serde_json::to_string(&partition_values)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        let intent = CatalogIntent {
            version: CATALOG_INTENT_VERSION,
            identity: CatalogIntentIdentity {
                sink_ref: match self.binding {
                    RuntimeBinding::Primary => "primary".to_string(),
                    RuntimeBinding::Deadletter => "deadletter".to_string(),
                },
                namespace: namespace.to_string(),
                kind: CatalogIntentKind::UpsertPartition,
                key,
            },
            payload,
        };
        if !intent.is_admissible() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Athena catalog intent for '{namespace}' is not admissible"),
            ));
        }
        Ok(Some(intent))
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
        idempotency_manifest: Option<&ObjectWriteManifest>,
    ) -> Result<(SinkWriteOutcome, Option<InnerSyncApplied>), std::io::Error> {
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

        // Partitioning: ReplacePartition may peek contract keys; Append trusts WAL filename only.
        let mut partition_values: Vec<String> = vec![];

        let contract_needs_peek = matches!(write_policy, WritePolicy::ReplacePartition)
            && source_contract.is_some_and(|contract| !contract.partition_key.is_empty());
        let (contract_peek_batch, stream) = if contract_needs_peek {
            Self::peek_first_batch(stream).await?
        } else {
            (None, stream)
        };

        if contract_needs_peek {
            if let (Some(contract), Some(batch)) = (source_contract, contract_peek_batch.as_ref()) {
                for (column, value) in contract_partition_key_values(contract, batch)? {
                    partition_values.push(value.clone());
                    tags.insert(column.clone(), value.clone());
                    full_key = format!("{}/{}={}", full_key, column, value);
                }
            }
        }

        let partition_path = BufferChunker::decode_file_partition(&filename);

        let append_requires_wal_partition = matches!(write_policy, WritePolicy::Append)
            && source_contract.is_some_and(|contract| !contract.partition_key.is_empty());
        if append_requires_wal_partition && partition_path.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Athena append for namespace {} has no WAL filename partition; configure pipeline batch_partition_fields so skipprd core routes rows into partitioned WAL segments before sink upload",
                    namespace
                ),
            ));
        }

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
        let has_contract_partition_scope = contract_needs_peek && contract_peek_batch.is_some();
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
        let (object_stem, final_key) = athena_object_key(&full_key, &filename, object_stem);
        let idempotency_manifest_key =
            sidecar_manifest_object_key(&self.config.s3_prefix, &namespace, &object_stem);
        let catalog_intents = match partition_metadata.as_ref() {
            Some(metadata) => self
                .partition_catalog_intent(
                    &namespace,
                    partition_values.clone(),
                    &full_key,
                    metadata,
                    source_contract,
                )
                .await?
                .into_iter()
                .collect(),
            None => Vec::new(),
        };
        if let Some(manifest) = idempotency_manifest {
            if self
                .manifest_matches(&idempotency_manifest_key, manifest)
                .await?
            {
                return Ok((
                    SinkWriteOutcome::AlreadyApplied,
                    Some(InnerSyncApplied {
                        final_key,
                        rows: 0,
                        bytes: 0,
                        etag: None,
                        checksum: None,
                        catalog_intents,
                    }),
                ));
            }
        }

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
        let permit = self
            .upload_sem
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "Semaphore closed"))?;
        let upload_start = std::time::Instant::now();
        let upload_metrics = UploadInFlightGuard::start();

        let bucket = self.config.s3_bucket.clone();

        // Stream Parquet to S3 via multipart upload
        debug!(
            "Uploader: start ns={} key_base={} filename={} bucket={}",
            namespace, full_key, filename, bucket
        );

        let key_for_upload = final_key.clone();

        // Resolve ordering. For bounded grouped writes, ordering is applied
        // within each incoming batch/row group only; no global sort metadata is emitted.
        let schema = stream.schema();
        let order_fields =
            skippr_runtime_sdk::converters::parquet_ordering::resolve_effective_order_from_fields(
                &schema,
                &self.context.output_layout.order_fields,
            );
        let row_group_size =
            skippr_runtime_sdk::converters::parquet_ordering::default_streaming_row_group_size();
        let props = skippr_runtime_sdk::converters::parquet_ordering::build_writer_properties(
            &schema,
            &order_fields,
            row_group_size,
        );
        let sorted_batches = stream.map({
            let order_fields = order_fields.clone();
            move |batch| {
                let batch = batch.map_err(ObjectWriteError::input)?;
                skippr_runtime_sdk::converters::parquet_ordering::sort_batch(&batch, &order_fields)
                    .map_err(ObjectWriteError::input)
            }
        });
        let backend = Arc::new(AthenaS3ObjectWriteBackend::new(
            self.s3_client.clone(),
            bucket.clone(),
            key_for_upload.clone(),
            tags_str,
        ));
        let request = ObjectWriteRequest {
            object_key: key_for_upload.clone(),
            content_type: "application/octet-stream".to_string(),
            metadata: BTreeMap::new(),
        };
        let writer_config = athena_object_writer_config();
        let receipt = ObjectWriteSession::new(backend, request, writer_config)
            .map_err(|error| io::Error::other(error.to_string()))?
            .write_parquet(schema, props, sorted_batches)
            .await
            .map_err(|error| io::Error::other(error.to_string()))?;
        let rows_written = receipt.rows;
        let uploaded_bytes = receipt.bytes;
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
            receipt.upload_id,
            receipt.parts.len(),
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
        // Release upload concurrency before Glue catalog work (queryability, not durability).
        drop(upload_metrics);
        drop(permit);
        info!(
            "s3_upload_complete ns={} key={} rows={} bytes={} upload_elapsed_ms={}",
            namespace,
            final_key,
            rows_written,
            uploaded_bytes,
            upload_start.elapsed().as_millis()
        );
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
            tokio::spawn(async move {
                crate::helpers::manifest::Manifest::ensure_prefix_and_db(
                    &crate::helpers::configuration::Config::new(),
                    &ns,
                    &prefix_for_manifest,
                    "",
                )
                .await;
            });
        }

        if let Some(manifest) = idempotency_manifest {
            self.write_manifest(&idempotency_manifest_key, manifest)
                .await?;
        }
        Ok((
            SinkWriteOutcome::Applied,
            Some(InnerSyncApplied {
                final_key: final_key.clone(),
                rows: rows_written,
                bytes: uploaded_bytes,
                etag: receipt.etag,
                checksum: receipt.checksum,
                catalog_intents,
            }),
        ))
    }

    async fn manifest_matches(
        &self,
        manifest_key: &str,
        expected: &ObjectWriteManifest,
    ) -> io::Result<bool> {
        let response = match self
            .s3_client
            .get_object()
            .bucket(&self.config.s3_bucket)
            .key(manifest_key)
            .send()
            .await
        {
            Ok(response) => response,
            Err(err) => {
                if is_s3_get_object_not_found_error(&err) {
                    return Ok(false);
                }
                return Err(io::Error::other(format!(
                    "Failed to read Athena idempotency manifest {}: {}",
                    manifest_key, err
                )));
            }
        };
        let bytes = response
            .body
            .collect()
            .await
            .map_err(|err| io::Error::other(err.to_string()))?
            .into_bytes();
        let manifest = ObjectWriteManifest::from_json_bytes(&bytes)?;
        Ok(manifest.matches_manifest(expected))
    }

    async fn write_manifest(
        &self,
        manifest_key: &str,
        manifest: &ObjectWriteManifest,
    ) -> io::Result<()> {
        self.s3_client
            .put_object()
            .bucket(&self.config.s3_bucket)
            .key(manifest_key)
            .body(ByteStream::from(manifest.to_json_bytes()?))
            .send()
            .await
            .map_err(|err| {
                io::Error::other(format!(
                    "Failed to write Athena idempotency manifest {}: {}",
                    manifest_key, err
                ))
            })?;
        Ok(())
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
                Ok(v) => {
                    maybe_restore_glue_cp_target();
                    return Ok(v);
                }
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
                        note_glue_transient_retry();
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
        let _cp_permit = acquire_glue_cp_permit().await;

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
                                None,
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
                let existing_partition_keys = table
                    .table()
                    .and_then(|t| t.partition_keys.clone())
                    .unwrap_or_default();
                if try_heal_glue_partition_layout(
                    context,
                    binding,
                    &config,
                    namespace,
                    schema,
                    source_contract,
                    &existing_partition_keys,
                    None,
                )
                .await
                .map_err(io::Error::other)?
                {
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
                            None,
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
        } else if input.trim().eq_ignore_ascii_case("exit") {
            return Err(format!(
                "Drop canceled. Exiting without dropping database '{}'.",
                database_name
            ));
        } else {
            return Err(format!(
                "Incorrect database name entered: '{}'.",
                input.trim()
            ));
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
            let clean_field_name = Helpers::clean_field_name(
                &crate::helpers::configuration::Config::new(),
                entity_name.to_string(),
            );

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
        partition_columns_override: Option<&[Column]>,
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

        let partitions = build_glue_partition_keys(
            context,
            binding,
            namespace,
            source_contract,
            metadata,
            partition_columns_override,
        );
        let mut partition_indexes: Vec<PartitionIndex> = Vec::new();
        let mut partition_index_keys: Vec<String> = Vec::new();
        if !granularity_target.is_empty() {
            for granularity in AwsAthena::time_partition_names_for_layout(&context.output_layout) {
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

fn glue_partition_column_names(columns: &[Column]) -> Vec<String> {
    columns
        .iter()
        .map(|column| column.name().to_string())
        .collect()
}

fn glue_partition_column_layout(columns: &[Column]) -> Vec<(String, String)> {
    columns
        .iter()
        .map(|column| {
            (
                column.name().to_string(),
                column.r#type().unwrap_or("string").to_string(),
            )
        })
        .collect()
}

fn partition_layout_mismatch(existing: &[Column], expected: &[Column]) -> bool {
    glue_partition_column_layout(existing) != glue_partition_column_layout(expected)
}

fn build_glue_partition_keys(
    context: &RuntimeExecutionContext,
    binding: RuntimeBinding,
    namespace: &str,
    source_contract: Option<&SourceNamespaceContract>,
    metadata: &OutputMetadata,
    partition_columns_override: Option<&[Column]>,
) -> Vec<Column> {
    let is_deadletter = binding == RuntimeBinding::Deadletter
        && namespace
            == skippr_runtime_sdk::sink_compat::deadletter::table_name(
                &skippr_runtime_sdk::helpers::configuration::Config::new(),
            );
    if is_deadletter {
        return Vec::new();
    }

    let mut partitions = Vec::new();
    if let Some(override_columns) = partition_columns_override {
        partitions.extend(override_columns.iter().cloned());
    } else if let Some(contract) = source_contract {
        append_contract_glue_partition_keys(&mut partitions, contract, metadata);
        AwsAthena::get_partition_by_fields(&context.output_layout, &mut partitions);
    } else {
        AwsAthena::get_partition_by_fields(&context.output_layout, &mut partitions);
    }

    let granularity_target = context
        .output_layout
        .time_partition_granularity
        .as_deref()
        .unwrap_or("");
    if !granularity_target.is_empty() {
        for granularity in AwsAthena::time_partition_names_for_layout(&context.output_layout) {
            partitions.push(
                Column::builder()
                    .name(granularity.to_string())
                    .r#type("int")
                    .build()
                    .unwrap(),
            );
            if granularity == granularity_target {
                break;
            }
        }
    }
    partitions
}

async fn glue_has_registered_partitions(
    glue_client: &GlueClient,
    database: &str,
    table_name: &str,
) -> Result<bool, String> {
    match glue_client
        .get_partitions()
        .database_name(database)
        .table_name(table_name)
        .max_results(1)
        .send()
        .await
    {
        Ok(output) => Ok(!output.partitions().is_empty()),
        Err(err) => Err(err.into_service_error().to_string()),
    }
}

async fn try_heal_glue_partition_layout(
    context: &RuntimeExecutionContext,
    binding: RuntimeBinding,
    config: &DataSinkAthenaPluginConfig,
    namespace: &str,
    metadata: &OutputMetadata,
    source_contract: Option<&SourceNamespaceContract>,
    existing_partition_keys: &[Column],
    partition_columns_override: Option<&[Column]>,
) -> Result<bool, String> {
    if is_deadletter_athena_target(binding, namespace) {
        return Ok(false);
    }

    let expected_partition_keys = build_glue_partition_keys(
        context,
        binding,
        namespace,
        source_contract,
        metadata,
        partition_columns_override,
    );
    if !partition_layout_mismatch(existing_partition_keys, &expected_partition_keys) {
        return Ok(false);
    }

    let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .load()
        .await;
    let glue_client = GlueClient::new(&aws_config);
    if glue_has_registered_partitions(&glue_client, &config.glue_database_name, namespace).await? {
        return Ok(false);
    }

    let existing_names = glue_partition_column_names(existing_partition_keys);
    let expected_names = glue_partition_column_names(&expected_partition_keys);
    info!(
        "Rebuilding Glue table '{}.{}' to heal partition layout: {:?} -> {:?}",
        config.glue_database_name, namespace, existing_names, expected_names
    );

    AwsAthena::backoff_retry(
        || AwsAthena::glue_delete_table(config, namespace),
        "delete_table",
    )
    .await?;
    AwsAthena::backoff_retry(
        || {
            AwsAthena::glue_create_table(
                context,
                binding,
                config,
                namespace,
                metadata,
                source_contract,
                partition_columns_override,
            )
        },
        "create_table",
    )
    .await?;
    Ok(true)
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
    use arrow::array::{Int32Array, StringArray};
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
    fn unpublished_schema_version_cannot_mint_catalog_intent() {
        assert_eq!(
            published_catalog_schema_version(&BTreeMap::new(), "events"),
            None
        );
        assert_eq!(
            published_catalog_schema_version(&BTreeMap::from([("events".into(), 0)]), "events"),
            None
        );
        assert_eq!(
            published_catalog_schema_version(&BTreeMap::from([("events".into(), 3)]), "events"),
            NonZeroU64::new(3)
        );
        let err = catalog_intent_schema_version(&BTreeMap::from([("events".into(), 0)]), "events")
            .unwrap_err();
        assert!(err.to_string().contains("installed schema version"));
        assert_eq!(
            catalog_intent_schema_version(&BTreeMap::from([("events".into(), 3)]), "events",)
                .unwrap(),
            NonZeroU64::new(3).unwrap()
        );
    }

    #[test]
    fn equal_or_older_schema_state_install_is_a_noop() {
        let mut state = InstalledAthenaSchemaState::default();
        let initial = BTreeMap::from([("events".to_string(), deadletter_output_metadata())]);
        let replacement = BTreeMap::from([("users".to_string(), deadletter_output_metadata())]);

        state.install(0, &initial);
        state.install(0, &replacement);
        assert_eq!(state.version, 0);
        assert!(state.namespaces.contains_key("events"));
        assert!(!state.namespaces.contains_key("users"));

        state.install(2, &replacement);
        state.install(1, &initial);
        assert_eq!(state.version, 2);
        assert!(state.namespaces.contains_key("users"));
        assert!(!state.namespaces.contains_key("events"));
    }

    #[test]
    fn schema_delta_uses_each_namespace_version_and_merges_only_changes() {
        let initial = deadletter_output_metadata();
        let changed = OutputMetadata::new();
        let mut state = InstalledAthenaSchemaState::default();
        state.install_snapshot(&RuntimeSchemaState {
            version: 7,
            namespaces: BTreeMap::from([("events".to_string(), initial.clone())]),
            namespace_versions: BTreeMap::from([("events".to_string(), 3)]),
        });

        let applied = state.install_delta(&SchemaDelta {
            version: 99,
            namespaces: BTreeMap::from([
                (
                    "events".to_string(),
                    skippr_runtime_sdk::protocol::SchemaNamespaceDelta {
                        version: 3,
                        metadata: changed.clone(),
                    },
                ),
                (
                    "users".to_string(),
                    skippr_runtime_sdk::protocol::SchemaNamespaceDelta {
                        version: 1,
                        metadata: changed,
                    },
                ),
            ]),
        });

        assert_eq!(applied, 1);
        assert_eq!(state.namespaces["events"], initial);
        assert_eq!(state.namespace_versions["events"], 3);
        assert_eq!(state.namespace_versions["users"], 1);
    }

    fn wat_append_partition_contract() -> SourceNamespaceContract {
        SourceNamespaceContract {
            namespace: "cc_wat_source_pages_by_target_domain_index".to_string(),
            primary_key: vec![
                FieldPath::single("crawl_id"),
                FieldPath::single("target_domain_hash_bucket"),
                FieldPath::single("target_domain_id"),
                FieldPath::single("source_url_id"),
            ],
            cursor: Some(FieldPath::single("wat_path")),
            partition_key: vec![
                FieldPath::single("crawl_id"),
                FieldPath::single("target_domain_hash_bucket"),
            ],
            write_policy: WritePolicy::Append,
            refresh_window: None,
            description: String::new(),
            semantics: None,
        }
    }

    fn wat_partition_batch(crawl_id: &str, bucket: i32) -> RecordBatch {
        let schema = Arc::new(Schema::new(vec![
            Field::new("crawl_id", DataType::Utf8, false),
            Field::new("target_domain_hash_bucket", DataType::Int32, false),
        ]));
        let crawl_ids = StringArray::from(vec![crawl_id]);
        let buckets = Int32Array::from(vec![bucket]);
        RecordBatch::try_new(schema, vec![Arc::new(crawl_ids), Arc::new(buckets)]).unwrap()
    }

    #[test]
    fn shared_writer_preserves_legacy_and_grouped_object_keys() {
        let filename = "namespace=users-p=region%3Deu-c=segment-1";
        let legacy_stem = hex::encode(md5::compute(filename).0);
        assert_eq!(
            athena_object_key("root/users/region=eu", filename, None),
            (
                legacy_stem.clone(),
                format!("root/users/region=eu/{legacy_stem}.parquet")
            )
        );
        assert_eq!(
            athena_object_key("root/users/region=eu", filename, Some("compaction-0001")),
            (
                "compaction-0001".to_string(),
                "root/users/region=eu/compaction-0001.parquet".to_string()
            )
        );
    }

    #[test]
    fn grouped_receipt_fields_match_the_completed_shared_write() {
        let manifest = ObjectWriteManifest {
            compaction_id: "compaction-0001".to_string(),
            idempotency_key: "apply-0001".to_string(),
            schema_fingerprint: "schema-1".to_string(),
            wal_refs_fingerprint: "legacy-wal-fingerprint".to_string(),
            wal_ref_count: 2,
            identity_version: Some(2),
            wal_refs_fingerprint_v2: Some("canonical-wal-fingerprint".to_string()),
            has_cdc_metadata: true,
            wal_segment_ids: Vec::new(),
        };
        let applied = InnerSyncApplied {
            final_key: "root/users/region=eu/compaction-0001.parquet".to_string(),
            rows: 123,
            bytes: 456,
            etag: Some("\"multipart-etag-2\"".to_string()),
            checksum: None,
            catalog_intents: Vec::new(),
        };

        let receipt = grouped_receipt_from_applied(
            &manifest,
            "warehouse",
            &applied,
            "\"multipart-etag-2\"".to_string(),
            3,
        );

        assert!(receipt.matches_manifest(&manifest));
        assert_eq!(
            receipt.final_s3_key,
            "s3://warehouse/root/users/region=eu/compaction-0001.parquet"
        );
        assert_eq!(receipt.etag, "\"multipart-etag-2\"");
        assert_eq!(receipt.checksum, None);
        assert_eq!(receipt.rows, 123);
        assert_eq!(receipt.bytes, 456);
        assert_eq!(receipt.transport_chunk_count, 3);
    }

    #[test]
    fn athena_shared_writer_has_explicit_part_and_memory_bounds() {
        let config = athena_object_writer_config();
        assert_eq!(config.part_size, 16 * 1024 * 1024);
        assert!((1..=8).contains(&config.max_in_flight_parts));
        assert_eq!(config.batch_channel_capacity, 1);
        assert_eq!(config.byte_channel_capacity, 1);
        assert_eq!(
            config.transport_memory_bound_bytes(),
            config.part_size * (config.max_in_flight_parts + 2)
        );
    }

    #[test]
    fn partition_layout_mismatch_detects_empty_vs_time_partitions() {
        let existing: Vec<Column> = vec![];
        let expected = vec![
            Column::builder()
                .name("year")
                .r#type("int")
                .build()
                .unwrap(),
            Column::builder()
                .name("month")
                .r#type("int")
                .build()
                .unwrap(),
            Column::builder().name("day").r#type("int").build().unwrap(),
        ];
        assert!(partition_layout_mismatch(&existing, &expected));
        assert!(!partition_layout_mismatch(&expected, &expected));
        let same_names_wrong_types = vec![
            Column::builder()
                .name("year")
                .r#type("string")
                .build()
                .unwrap(),
            Column::builder()
                .name("month")
                .r#type("string")
                .build()
                .unwrap(),
            Column::builder()
                .name("day")
                .r#type("string")
                .build()
                .unwrap(),
        ];
        assert!(partition_layout_mismatch(
            &same_names_wrong_types,
            &expected
        ));
    }

    #[test]
    fn build_glue_partition_keys_includes_day_granularity() {
        use skippr_runtime_sdk::protocol::{
            RuntimeExecutionContext, RuntimeExecutionMode, RuntimeOutputLayout,
        };
        let context = RuntimeExecutionContext {
            pipeline_name: "device_data".to_string(),
            workspace_name: "prod".to_string(),
            data_dir: "/tmp".to_string(),
            execution_mode: RuntimeExecutionMode::Sync,
            output_layout: RuntimeOutputLayout {
                partition_fields: vec![],
                order_fields: vec![],
                time_partition_granularity: Some("day".to_string()),
                time_partition_prefix: None,
            },
            inject_fields: Default::default(),
        };
        let keys = build_glue_partition_keys(
            &context,
            RuntimeBinding::Primary,
            "device_data",
            None,
            &OutputMetadata::new(),
            None,
        );
        assert_eq!(
            glue_partition_column_names(&keys),
            vec!["year", "month", "day"]
        );
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
    fn append_contract_partition_key_values_match_wat_hive_layout() {
        let contract = wat_append_partition_contract();
        let batch = wat_partition_batch("CC-MAIN-2025-08", 12345);
        let values = contract_partition_key_values(&contract, &batch).unwrap();
        assert_eq!(
            values,
            vec![
                ("crawl_id".to_string(), "CC-MAIN-2025-08".to_string()),
                ("target_domain_hash_bucket".to_string(), "12345".to_string()),
            ]
        );
        let path = values
            .into_iter()
            .map(|(column, value)| format!("{column}={value}"))
            .collect::<Vec<_>>()
            .join("/");
        assert_eq!(
            path,
            "crawl_id=CC-MAIN-2025-08/target_domain_hash_bucket=12345"
        );
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
    fn append_wal_filename_partition_decodes_from_encoded_chunk_name() {
        let filename = BufferChunker::encode_chunk_name(
            "part-0001",
            None,
            Some("cc_wat_source_pages_by_target_domain_index"),
            Some("p_crawl_id=cc_main_x/p_target_domain_hash_bucket=item_1"),
            None,
            None,
        );
        let partition_path = BufferChunker::decode_file_partition(&filename);
        assert_eq!(
            partition_path,
            "p_crawl_id=cc_main_x/p_target_domain_hash_bucket=item_1"
        );
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

fn is_s3_not_found_error(err: &impl ProvideErrorMetadata) -> bool {
    matches!(
        err.code(),
        Some("NoSuchKey") | Some("NotFound") | Some("404")
    )
}

fn is_s3_get_object_not_found_error(err: &S3SdkError<GetObjectError>) -> bool {
    if err
        .raw_response()
        .is_some_and(|response| response.status().as_u16() == 404)
    {
        return true;
    }
    if let Some(service_error) = err.as_service_error() {
        if is_s3_not_found_code(service_error.code()) {
            return true;
        }
        if service_error
            .message()
            .is_some_and(is_s3_not_found_error_text)
        {
            return true;
        }
    }
    is_s3_not_found_error_text(&err.to_string())
}

fn is_s3_not_found_code(code: Option<&str>) -> bool {
    matches!(
        code,
        Some("NoSuchKey" | "NotFound" | "NotFoundException" | "404")
    )
}

fn is_s3_not_found_error_text(err: &str) -> bool {
    err.contains("NoSuchKey")
        || err.contains("NotFound")
        || err.contains("status code: 404")
        || err.contains("404 Not Found")
}

/// Extract hive partition prefix + values from an object key / s3 URI.
fn partition_values_from_object_key(
    namespace: &str,
    final_s3_key: &str,
) -> Option<(String, Vec<String>)> {
    let key = final_s3_key
        .trim_start_matches("s3://")
        .split_once('/')
        .map(|(_, rest)| rest)
        .unwrap_or(final_s3_key);
    let key = key.trim_end_matches('/').trim_end_matches(".parquet");
    let prefix = key.rsplit_once('/')?.0;
    let mut partition_values = Vec::new();
    let mut full_key_parts = Vec::new();
    let mut seen_ns = false;
    for part in prefix.split('/') {
        if !seen_ns {
            if part == namespace {
                seen_ns = true;
            }
            full_key_parts.push(part.to_string());
            continue;
        }
        if let Some((_k, v)) = part.split_once('=') {
            partition_values.push(v.to_string());
            full_key_parts.push(part.to_string());
        }
    }
    if partition_values.is_empty() {
        return None;
    }
    Some((full_key_parts.join("/"), partition_values))
}

#[cfg(test)]
mod partition_schedule_tests {
    use super::partition_values_from_object_key;

    #[test]
    fn extracts_hive_partitions_from_s3_uri() {
        let (full_key, values) = partition_values_from_object_key(
            "truck_status_idle",
            "s3://bucket/prefix/truck_status_idle/year=2024/month=01/abcd1234",
        )
        .expect("partitions");
        assert_eq!(full_key, "prefix/truck_status_idle/year=2024/month=01");
        assert_eq!(values, vec!["2024".to_string(), "01".to_string()]);
    }

    #[test]
    fn returns_none_without_hive_partitions() {
        assert!(partition_values_from_object_key(
            "truck_status_idle",
            "s3://bucket/prefix/truck_status_idle/abcd1234.parquet",
        )
        .is_none());
    }
}
