use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll as TaskPoll};
use std::time::Duration;

use arrow::array::{Array, ArrayRef, BooleanArray, RecordBatch, StringArray};
use arrow::compute::filter_record_batch;
use arrow::datatypes::{Schema as ArrowSchema, SchemaRef};
use arrow::util::display::array_value_to_string;
use async_trait::async_trait;
use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_s3::operation::get_object::GetObjectError;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use aws_sdk_s3::Client as S3Client;
use datafusion::error::DataFusionError;
use datafusion::execution::SendableRecordBatchStream;
use datafusion::physical_plan::RecordBatchStream;
use futures::stream::FuturesUnordered;
use futures::Stream;
use futures::StreamExt;
use iceberg::spec::{
    DataContentType, DataFileBuilder, DataFileFormat, ListType, MapType, NestedField,
    PrimitiveType, Schema, Struct, Type,
};
use iceberg::transaction::{ApplyTransactionAction, Transaction};
use iceberg::{Catalog, CatalogBuilder, NamespaceIdent, TableCreation, TableIdent};
use iceberg_catalog_glue::{GlueCatalog, GlueCatalogBuilder, GLUE_CATALOG_PROP_CATALOG_ID};
use iceberg_catalog_glue::{AWS_REGION_NAME, GLUE_CATALOG_PROP_WAREHOUSE};
use parquet::arrow::PARQUET_FIELD_ID_META_KEY;
use serde_derive::{Deserialize, Serialize};
use skippr_object_writer::{
    CompletionMetadata, MultipartUpload, ObjectPartReceipt, ObjectWriteBackend, ObjectWriteError,
    ObjectWriteRequest, ObjectWriteSession, ObjectWriterConfig, PartMetadata,
};
use tokio::sync::{Mutex, RwLock};
use tracing::{info, warn};

const ICEBERG_FILE_MAX_ROWS: usize = 100_000;
const ICEBERG_FILE_MAX_INPUT_BYTES: usize = 64 * 1024 * 1024;
const ICEBERG_MAX_FILE_WRITES_IN_FLIGHT: usize = 4;
const ICEBERG_CDC_STATE_SHARDS: u8 = 64;
const ICEBERG_MAX_ENVELOPE_KEYS: usize = 1_000_000;
const ICEBERG_GROUPED_PENDING_VERSION: u32 = 2;
const SNAPSHOT_COMPACTION_ID: &str = "skippr.compaction-id";
const SNAPSHOT_IDEMPOTENCY_KEY: &str = "skippr.idempotency-key";
const SNAPSHOT_SCHEMA_FINGERPRINT: &str = "skippr.schema-fingerprint";
const SNAPSHOT_WAL_FINGERPRINT: &str = "skippr.wal-refs-fingerprint-v2";
const SNAPSHOT_WAL_REF_COUNT: &str = "skippr.wal-ref-count";

const ICEBERG_WRITE_POLICY_SUPPORT: SinkWritePolicySupport = SinkWritePolicySupport {
    supports_merge_by_key: true,
    supports_replace_partition: true,
    supports_replace_table: true,
};

use crate::helpers::configuration::DataSinkPluginConfig;
use skippr_runtime_sdk::discover::{OutputMetadata, SkipprDataType};
use skippr_runtime_sdk::plugins::cdc::EffectiveGuarantee;
use skippr_runtime_sdk::plugins::source_contract::{
    ensure_source_contract_for_policy, namespace_source_contract, validate_write_policy_for_sink,
    FieldPath, SinkWritePolicySupport, SourceNamespaceContract, WritePolicy,
};
use skippr_runtime_sdk::plugins::{DataSink, SchemaSink, SinkWriteContext, SinkWriteOutcome};
use skippr_runtime_sdk::protocol::{RuntimeBinding, RuntimeExecutionContext};
use skippr_runtime_sdk::sink_compat::BufferChunker;
use skippr_runtime_sdk::sink_idempotency::{manifest_object_name, ObjectWriteManifest};

#[derive(Debug, Deserialize, Clone)]
pub struct DataSinkIcebergPluginConfig {
    pub catalog: IcebergCatalogConfig,
    #[serde(default)]
    pub table_namespace: Option<String>,
    #[serde(default)]
    pub table_prefix: Option<String>,
    #[serde(default)]
    pub table_location_prefix: Option<String>,
    #[serde(default)]
    pub properties: BTreeMap<String, String>,
    #[serde(default)]
    pub query_engine: Option<IcebergQueryEngineConfig>,
    #[serde(default)]
    pub format: Option<String>,
}

impl TryFrom<DataSinkPluginConfig> for DataSinkIcebergPluginConfig {
    type Error = String;

    fn try_from(entry: DataSinkPluginConfig) -> Result<Self, Self::Error> {
        entry.decode_for_plugin("Iceberg")
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IcebergCatalogConfig {
    Glue {
        warehouse: String,
        #[serde(default)]
        database: Option<String>,
        #[serde(default)]
        catalog_id: Option<String>,
        #[serde(default)]
        region: Option<String>,
    },
    Rest {
        uri: String,
        warehouse: String,
    },
    Unity {
        uri: String,
        warehouse: String,
        #[serde(default)]
        token: Option<String>,
    },
    Polaris {
        uri: String,
        warehouse: String,
        #[serde(default)]
        client_id: Option<String>,
        #[serde(default)]
        client_secret: Option<String>,
    },
}

#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IcebergQueryEngineConfig {
    Athena {
        #[serde(default)]
        workgroup: Option<String>,
    },
}

#[derive(Default)]
struct InstalledIcebergSchemaState {
    installed: bool,
    version: u64,
    namespaces: BTreeMap<String, OutputMetadata>,
}

impl InstalledIcebergSchemaState {
    fn install(
        &mut self,
        schema_version: u64,
        namespaces: &BTreeMap<String, OutputMetadata>,
    ) -> bool {
        if self.installed && schema_version <= self.version {
            return false;
        }
        self.installed = true;
        self.version = schema_version;
        self.namespaces = namespaces.clone();
        true
    }
}

#[derive(Clone)]
struct CachedIcebergTable {
    schema_version: u64,
    table: iceberg::table::Table,
}

pub struct DataSinkIcebergPlugin {
    context: RuntimeExecutionContext,
    binding: RuntimeBinding,
    #[allow(dead_code)]
    buffer_name: String,
    config: DataSinkIcebergPluginConfig,
    s3_client: S3Client,
    schema_state: RwLock<InstalledIcebergSchemaState>,
    catalog_cache: RwLock<Option<Arc<GlueCatalog>>>,
    table_cache: RwLock<HashMap<String, CachedIcebergTable>>,
    table_lanes: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum PendingFileContent {
    Data,
    EqualityDeletes,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PendingIcebergFile {
    content: PendingFileContent,
    file_uri: String,
    rows: u64,
    bytes: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    equality_ids: Vec<i32>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct IcebergGroupedPending {
    version: u32,
    manifest: ObjectWriteManifest,
    namespace: String,
    schema_version: u64,
    transport_chunk_count: u32,
    rows: u64,
    files: Vec<PendingIcebergFile>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    state_delta: BTreeMap<String, String>,
}

struct GroupedCommitResult {
    table: iceberg::table::Table,
    already_committed: bool,
}

#[derive(Default)]
struct CdcStateSession {
    loaded_shards: HashMap<u8, LoadedCdcStateShard>,
    legacy_state: Option<HashMap<String, String>>,
    legacy_checked: bool,
    delta: BTreeMap<String, String>,
}

struct LoadedCdcStateShard {
    values: BTreeMap<String, String>,
    etag: Option<String>,
}

#[derive(Clone)]
struct S3ObjectWriteBackend {
    client: S3Client,
    bucket: String,
}

impl S3ObjectWriteBackend {
    fn upload_key<'a>(&self, upload: &'a MultipartUpload) -> Result<&'a str, io::Error> {
        upload
            .metadata
            .get("object_key")
            .map(String::as_str)
            .ok_or_else(|| io::Error::other("S3 multipart upload is missing object_key metadata"))
    }
}

#[async_trait]
impl ObjectWriteBackend for S3ObjectWriteBackend {
    type Error = io::Error;

    async fn begin(&self, request: &ObjectWriteRequest) -> Result<MultipartUpload, Self::Error> {
        let output = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(&request.object_key)
            .content_type(&request.content_type)
            .send()
            .await
            .map_err(|err| io::Error::other(format!("create multipart upload failed: {err}")))?;
        let upload_id = output
            .upload_id()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| io::Error::other("S3 create multipart upload returned no upload id"))?
            .to_string();
        Ok(MultipartUpload {
            upload_id,
            metadata: BTreeMap::from([("object_key".to_string(), request.object_key.clone())]),
        })
    }

    async fn upload_part(
        &self,
        upload: &MultipartUpload,
        part_number: u32,
        bytes: bytes::Bytes,
    ) -> Result<PartMetadata, Self::Error> {
        let part_number = i32::try_from(part_number)
            .map_err(|_| io::Error::other("S3 multipart part number overflowed i32"))?;
        let output = self
            .client
            .upload_part()
            .bucket(&self.bucket)
            .key(self.upload_key(upload)?)
            .upload_id(&upload.upload_id)
            .part_number(part_number)
            .body(ByteStream::from(bytes))
            .send()
            .await
            .map_err(|err| io::Error::other(format!("upload multipart part failed: {err}")))?;
        Ok(PartMetadata {
            etag: output.e_tag().map(str::to_string),
            checksum: output.checksum_sha256().map(str::to_string),
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
                    .map_err(|_| io::Error::other("S3 multipart part number overflowed i32"))?;
                Ok(CompletedPart::builder()
                    .set_e_tag(part.etag.clone())
                    .part_number(part_number)
                    .build())
            })
            .collect::<Result<Vec<_>, io::Error>>()?;
        let output = self
            .client
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(self.upload_key(upload)?)
            .upload_id(&upload.upload_id)
            .multipart_upload(
                CompletedMultipartUpload::builder()
                    .set_parts(Some(completed_parts))
                    .build(),
            )
            .send()
            .await
            .map_err(|err| io::Error::other(format!("complete multipart upload failed: {err}")))?;
        Ok(CompletionMetadata {
            etag: output.e_tag().map(str::to_string),
            checksum: output.checksum_sha256().map(str::to_string),
            version_id: output.version_id().map(str::to_string),
            metadata: BTreeMap::new(),
        })
    }

    async fn abort(&self, upload: &MultipartUpload) -> Result<(), Self::Error> {
        self.client
            .abort_multipart_upload()
            .bucket(&self.bucket)
            .key(self.upload_key(upload)?)
            .upload_id(&upload.upload_id)
            .send()
            .await
            .map_err(|err| io::Error::other(format!("abort multipart upload failed: {err}")))?;
        Ok(())
    }
}

skippr_runtime_sdk::declare_sink_spec!(
    IcebergSinkSpec,
    DataSinkIcebergPlugin,
    skippr_runtime_sdk::plugins::cdc::sink_capabilities::ICEBERG,
    skippr_runtime_sdk::plugins::TransactionalTableCommit
);

#[async_trait]
impl DataSink for DataSinkIcebergPlugin {
    async fn sync(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
        cdc_ctx: Option<&skippr_runtime_sdk::plugins::cdc::SyncContext>,
    ) -> Result<(), io::Error> {
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
    ) -> Result<(), io::Error> {
        let stream = match ctx.cdc_ctx {
            Some(cdc) => super::cdc_encode::augment_stream_with_cdc_columns(stream, &cdc.part_meta),
            None => stream,
        };
        let object_stem = if ctx.idempotency_key.is_empty() {
            None
        } else {
            Some(ctx.idempotency_key.as_str())
        };
        if ctx.cdc_ctx.is_some() {
            return self
                .native_append(stream, ctx.filename, ctx.cdc_ctx, object_stem)
                .await;
        }
        let namespace = BufferChunker::decode_file_namespace(&ctx.filename);
        let resolved_contract = ctx
            .source_contract
            .cloned()
            .or_else(|| namespace_source_contract(&namespace));
        let policy = resolved_contract
            .as_ref()
            .map(|c| c.write_policy)
            .unwrap_or(WritePolicy::Append);
        if let Some(ref contract) = resolved_contract {
            validate_write_policy_for_sink(contract, "Iceberg", ICEBERG_WRITE_POLICY_SUPPORT)
                .map_err(|err| io::Error::new(io::ErrorKind::Unsupported, err.to_string()))?;
        }
        ensure_source_contract_for_policy(&namespace, policy, resolved_contract.as_ref())?;
        match policy {
            WritePolicy::Append => {
                self.native_append(stream, ctx.filename, None, object_stem)
                    .await
            }
            WritePolicy::MergeByKey | WritePolicy::ReplacePartition | WritePolicy::ReplaceTable => {
                let contract = resolved_contract.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "Iceberg write policy {:?} requires a source namespace contract",
                            policy
                        ),
                    )
                })?;
                self.native_policy_write(stream, ctx.filename, &contract, policy, object_stem)
                    .await
            }
        }
    }

    async fn sync_with_context_result(
        &self,
        stream: SendableRecordBatchStream,
        ctx: SinkWriteContext<'_>,
    ) -> Result<SinkWriteOutcome, io::Error> {
        if !ctx.is_grouped() {
            self.sync_with_context(stream, ctx).await?;
            return Ok(SinkWriteOutcome::Applied);
        }
        ctx.validate_grouped::<skippr_runtime_sdk::plugins::TransactionalTableCommit>()
            .map_err(|err| io::Error::new(io::ErrorKind::Unsupported, err))?;
        let namespace = BufferChunker::decode_file_namespace(&ctx.filename);
        let manifest = ObjectWriteManifest::from_context(
            ctx.compaction_id.clone(),
            ctx.idempotency_key.clone(),
            ctx.schema_fingerprint.clone(),
            &ctx.wal_refs,
        );
        let (bucket, key) = self.idempotency_manifest_location(&namespace, &ctx.idempotency_key)?;
        if self.manifest_matches(&bucket, &key, &manifest).await? {
            return Ok(SinkWriteOutcome::AlreadyApplied);
        }
        self.sync_with_context(stream, ctx).await?;
        self.write_manifest(&bucket, &key, &manifest).await?;
        Ok(SinkWriteOutcome::Applied)
    }

    async fn sync_grouped(
        &self,
        reader: skippr_runtime_sdk::plugins::GroupedBatchReader,
        ctx: skippr_runtime_sdk::plugins::GroupedSinkWriteContext<'_>,
    ) -> Result<SinkWriteOutcome, io::Error> {
        ctx.to_sink_write_context()
            .validate_grouped::<skippr_runtime_sdk::plugins::TransactionalTableCommit>()
            .map_err(|err| io::Error::new(io::ErrorKind::Unsupported, err))?;
        let namespace = BufferChunker::decode_file_namespace(&ctx.filename);
        let manifest = ObjectWriteManifest::from_context(
            ctx.compaction_id.clone(),
            ctx.idempotency_key.clone(),
            ctx.schema_fingerprint.clone(),
            &ctx.wal_refs.clone_vec(),
        );
        let (manifest_bucket, manifest_key) =
            self.idempotency_manifest_location(&namespace, &ctx.idempotency_key)?;
        if let Some(existing) = self.read_manifest(&manifest_bucket, &manifest_key).await? {
            if existing.matches_manifest(&manifest) {
                return Ok(SinkWriteOutcome::AlreadyApplied);
            }
            return Err(io::Error::other(format!(
                "Iceberg grouped manifest mismatch for compaction '{}'",
                ctx.compaction_id
            )));
        }

        if self
            .legacy_grouped_manifest_exists(&namespace, &ctx)
            .await?
        {
            let lane = self.table_lane(&namespace).await;
            let _lane_guard = lane.lock().await;
            if let Some(existing) = self.read_manifest(&manifest_bucket, &manifest_key).await? {
                if existing.matches_manifest(&manifest) {
                    return Ok(SinkWriteOutcome::AlreadyApplied);
                }
                return Err(io::Error::other(format!(
                    "Iceberg grouped manifest mismatch for compaction '{}'",
                    ctx.compaction_id
                )));
            }
            return self.sync_grouped_legacy_chunks(reader, ctx).await;
        }

        let pending_key = grouped_pending_key(&manifest_key);
        let mut reader = Some(reader);
        let mut prepared = self
            .read_grouped_pending(&manifest_bucket, &pending_key)
            .await?;
        if let Some(pending) = prepared.as_ref() {
            validate_grouped_pending(pending, &namespace, &manifest, &ctx.compaction_id)?;
        }
        if prepared.is_none() && !grouped_preparation_requires_lane(&ctx, &namespace) {
            prepared = Some(
                self.produce_grouped_pending(
                    reader
                        .take()
                        .ok_or_else(|| io::Error::other("grouped reader was already consumed"))?,
                    &ctx,
                    &namespace,
                    manifest.clone(),
                )
                .await?,
            );
        }

        let lane = self.table_lane(&namespace).await;
        let _lane_guard = lane.lock().await;
        if let Some(existing) = self.read_manifest(&manifest_bucket, &manifest_key).await? {
            if existing.matches_manifest(&manifest) {
                return Ok(SinkWriteOutcome::AlreadyApplied);
            }
            return Err(io::Error::other(format!(
                "Iceberg grouped manifest mismatch for compaction '{}'",
                ctx.compaction_id
            )));
        }
        let pending = match self
            .read_grouped_pending(&manifest_bucket, &pending_key)
            .await?
        {
            Some(pending) => {
                validate_grouped_pending(&pending, &namespace, &manifest, &ctx.compaction_id)?;
                pending
            }
            None => {
                let pending = match prepared {
                    Some(pending) => pending,
                    None => {
                        self.produce_grouped_pending(
                            reader.take().ok_or_else(|| {
                                io::Error::other("grouped reader was already consumed")
                            })?,
                            &ctx,
                            &namespace,
                            manifest.clone(),
                        )
                        .await?
                    }
                };
                self.write_grouped_pending(&manifest_bucket, &pending_key, &pending)
                    .await?;
                pending
            }
        };
        let already_committed = self.apply_grouped_pending(&namespace, &pending).await?;
        self.write_manifest(&manifest_bucket, &manifest_key, &manifest)
            .await?;
        Ok(if already_committed {
            SinkWriteOutcome::AlreadyApplied
        } else {
            SinkWriteOutcome::Applied
        })
    }

    fn capability(&self) -> &'static skippr_runtime_sdk::plugins::cdc::SinkCapability {
        &skippr_runtime_sdk::plugins::cdc::sink_capabilities::ICEBERG
    }

    async fn install_schema_state(
        &self,
        schema_version: u64,
        namespaces: &BTreeMap<String, OutputMetadata>,
    ) -> Result<(), io::Error> {
        let invalidate = {
            let mut guard = self.schema_state.write().await;
            let previous_version = guard.version;
            if !guard.install(schema_version, namespaces) {
                return Ok(());
            }
            schema_version_invalidates_table_cache(previous_version, schema_version)
        };
        if invalidate {
            self.table_cache.write().await.clear();
        }
        Ok(())
    }
}

#[async_trait]
impl SchemaSink for DataSinkIcebergPlugin {
    async fn sync_schema(
        &self,
        namespace: &str,
        metadata: &OutputMetadata,
    ) -> Result<(), io::Error> {
        let catalog = self.glue_catalog().await?;
        let table = self.ensure_table(&catalog, namespace, metadata).await?;
        let schema_version = self.schema_state.read().await.version;
        self.table_cache.write().await.insert(
            namespace.to_string(),
            CachedIcebergTable {
                schema_version,
                table,
            },
        );
        Ok(())
    }
}

impl DataSinkIcebergPlugin {
    pub async fn new_with_config(
        context: RuntimeExecutionContext,
        binding: RuntimeBinding,
        buffer_name: String,
        config: DataSinkIcebergPluginConfig,
    ) -> io::Result<Self> {
        let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .load()
            .await;
        let s3_client = S3Client::new(&aws_config);
        Ok(Self {
            context,
            binding,
            buffer_name,
            config,
            s3_client,
            schema_state: RwLock::new(InstalledIcebergSchemaState::default()),
            catalog_cache: RwLock::new(None),
            table_cache: RwLock::new(HashMap::new()),
            table_lanes: Mutex::new(HashMap::new()),
        })
    }

    async fn table_lane(&self, namespace: &str) -> Arc<Mutex<()>> {
        let mut lanes = self.table_lanes.lock().await;
        lanes
            .entry(namespace.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    async fn namespace_schema_state(
        &self,
        namespace: &str,
    ) -> Result<(u64, OutputMetadata), io::Error> {
        let guard = self.schema_state.read().await;
        let metadata = guard.namespaces.get(namespace).cloned().ok_or_else(|| {
            io::Error::other(format!(
                "Iceberg sink schema state v{} is missing namespace '{}'",
                guard.version, namespace
            ))
        })?;
        Ok((guard.version, metadata))
    }

    async fn table_for_write(
        &self,
        namespace: &str,
        schema_version: u64,
        metadata: &OutputMetadata,
    ) -> Result<iceberg::table::Table, io::Error> {
        if let Some(cached) = self.table_cache.read().await.get(namespace).cloned() {
            if cached.schema_version == schema_version {
                return Ok(cached.table);
            }
        }
        let catalog = self.glue_catalog().await?;
        let table = self.ensure_table(&catalog, namespace, metadata).await?;
        self.table_cache.write().await.insert(
            namespace.to_string(),
            CachedIcebergTable {
                schema_version,
                table: table.clone(),
            },
        );
        Ok(table)
    }

    async fn recreate_table_for_grouped(
        &self,
        namespace: &str,
        schema_version: u64,
        metadata: &OutputMetadata,
    ) -> Result<iceberg::table::Table, io::Error> {
        let catalog = self.glue_catalog().await?;
        let table_ident = self.table_ident(namespace)?;
        if catalog
            .table_exists(&table_ident)
            .await
            .map_err(|err| io::Error::other(err.to_string()))?
        {
            catalog
                .drop_table(&table_ident)
                .await
                .map_err(|err| io::Error::other(err.to_string()))?;
        }
        let table = self.ensure_table(&catalog, namespace, metadata).await?;
        self.table_cache.write().await.insert(
            namespace.to_string(),
            CachedIcebergTable {
                schema_version,
                table: table.clone(),
            },
        );
        Ok(table)
    }

    async fn legacy_grouped_manifest_exists(
        &self,
        namespace: &str,
        ctx: &skippr_runtime_sdk::plugins::GroupedSinkWriteContext<'_>,
    ) -> io::Result<bool> {
        for identity in [&ctx.idempotency_key, &ctx.compaction_id] {
            let chunk_identity = format!("{identity}-chunk-00000000");
            let (bucket, key) = self.idempotency_manifest_location(namespace, &chunk_identity)?;
            if self.s3_object_exists(&bucket, &key).await? {
                return Ok(true);
            }
        }
        let table_location = self.table_location(namespace).ok_or_else(|| {
            io::Error::other("Iceberg sink requires table_location_prefix for legacy replay")
        })?;
        let (bucket, table_prefix) = parse_s3_uri(&table_location)?;
        for key in legacy_grouped_object_keys(&table_prefix, &ctx.idempotency_key) {
            if self.s3_object_exists(&bucket, &key).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn s3_object_exists(&self, bucket: &str, key: &str) -> io::Result<bool> {
        match self
            .s3_client
            .head_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(err)
                if err
                    .raw_response()
                    .is_some_and(|response| response.status().as_u16() == 404)
                    || is_s3_not_found_error_text(&err.to_string()) =>
            {
                Ok(false)
            }
            Err(err) => Err(io::Error::other(err.to_string())),
        }
    }

    async fn sync_grouped_legacy_chunks(
        &self,
        mut reader: skippr_runtime_sdk::plugins::GroupedBatchReader,
        ctx: skippr_runtime_sdk::plugins::GroupedSinkWriteContext<'_>,
    ) -> Result<SinkWriteOutcome, io::Error> {
        let schema = reader.schema();
        let mut applied = false;
        while let Some(chunk) = reader.next_chunk().await? {
            let chunk_cdc = ctx.chunk_cdc_context(&chunk)?;
            let chunk_ctx = ctx.chunk_sink_write_context_with_cdc(
                chunk.chunk_index,
                chunk.chunk_index == 0 && chunk.final_chunk,
                chunk_cdc.as_ref(),
            );
            if self
                .sync_with_context_result(chunk.into_stream(schema.clone()), chunk_ctx)
                .await?
                == SinkWriteOutcome::Applied
            {
                applied = true;
            }
        }
        Ok(if applied {
            SinkWriteOutcome::Applied
        } else {
            SinkWriteOutcome::AlreadyApplied
        })
    }

    async fn read_grouped_pending(
        &self,
        bucket: &str,
        key: &str,
    ) -> io::Result<Option<IcebergGroupedPending>> {
        let response = match self
            .s3_client
            .get_object()
            .bucket(bucket)
            .key(key)
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
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|err| io::Error::other(err.to_string()))
    }

    async fn write_grouped_pending(
        &self,
        bucket: &str,
        key: &str,
        pending: &IcebergGroupedPending,
    ) -> io::Result<()> {
        let bytes = serde_json::to_vec(pending).map_err(|err| io::Error::other(err.to_string()))?;
        self.s3_client
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(ByteStream::from(bytes))
            .send()
            .await
            .map_err(|err| io::Error::other(err.to_string()))?;
        Ok(())
    }

    async fn apply_grouped_pending(
        &self,
        namespace: &str,
        pending: &IcebergGroupedPending,
    ) -> Result<bool, io::Error> {
        let (installed_version, metadata) = self.namespace_schema_state(namespace).await?;
        if installed_version < pending.schema_version {
            return Err(io::Error::other(format!(
                "Iceberg grouped apply requires schema state v{} but only v{} is installed",
                pending.schema_version, installed_version
            )));
        }
        let table = self
            .table_for_write(namespace, installed_version, &metadata)
            .await?;
        let commit_files = pending
            .files
            .iter()
            .map(|file| {
                self.build_data_file(
                    &table,
                    match file.content {
                        PendingFileContent::Data => DataContentType::Data,
                        PendingFileContent::EqualityDeletes => DataContentType::EqualityDeletes,
                    },
                    file.file_uri.clone(),
                    file.rows,
                    file.bytes,
                    (!file.equality_ids.is_empty()).then(|| file.equality_ids.clone()),
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let catalog = self.glue_catalog().await?;
        let committed = self
            .commit_grouped_append_with_retries(
                &catalog,
                table.identifier().clone(),
                commit_files,
                &pending.manifest,
            )
            .await?;
        if !pending.state_delta.is_empty() {
            self.persist_cdc_state_delta(namespace, &pending.state_delta)
                .await?;
        }
        self.table_cache.write().await.insert(
            namespace.to_string(),
            CachedIcebergTable {
                schema_version: installed_version,
                table: committed.table,
            },
        );
        Ok(committed.already_committed)
    }

    async fn produce_grouped_pending(
        &self,
        mut reader: skippr_runtime_sdk::plugins::GroupedBatchReader,
        ctx: &skippr_runtime_sdk::plugins::GroupedSinkWriteContext<'_>,
        namespace: &str,
        manifest: ObjectWriteManifest,
    ) -> Result<IcebergGroupedPending, io::Error> {
        let (schema_version, metadata) = self.namespace_schema_state(namespace).await?;
        let resolved_contract = ctx
            .source_contract
            .cloned()
            .or_else(|| namespace_source_contract(namespace));
        let policy = if ctx.cdc_ctx.is_some() {
            WritePolicy::Append
        } else {
            resolved_contract
                .as_ref()
                .map(|contract| contract.write_policy)
                .unwrap_or(WritePolicy::Append)
        };
        if ctx.cdc_ctx.is_none() {
            if let Some(contract) = resolved_contract.as_ref() {
                validate_write_policy_for_sink(contract, "Iceberg", ICEBERG_WRITE_POLICY_SUPPORT)
                    .map_err(|err| io::Error::new(io::ErrorKind::Unsupported, err.to_string()))?;
            }
            ensure_source_contract_for_policy(namespace, policy, resolved_contract.as_ref())?;
        }
        let table = if policy == WritePolicy::ReplaceTable {
            self.recreate_table_for_grouped(namespace, schema_version, &metadata)
                .await?
        } else {
            self.table_for_write(namespace, schema_version, &metadata)
                .await?
        };
        let exact_cdc_contract = ctx
            .cdc_ctx
            .and_then(|cdc| cdc.contract.as_ref())
            .filter(|contract| {
                contract.effective_guarantee == EffectiveGuarantee::ExactOnceFinalState
            })
            .cloned();
        let equality_ids = if let Some(contract) = exact_cdc_contract.as_ref() {
            Some(equality_ids_for_columns(
                &table,
                namespace,
                &contract.business_key_columns,
            )?)
        } else {
            match policy {
                WritePolicy::MergeByKey => Some(plan_contract_equality_ids(
                    &table,
                    &resolved_contract
                        .as_ref()
                        .ok_or_else(|| {
                            io::Error::other(
                                "Iceberg MergeByKey requires a source namespace contract",
                            )
                        })?
                        .primary_key,
                )?),
                WritePolicy::ReplacePartition => Some(plan_contract_equality_ids(
                    &table,
                    &resolved_contract
                        .as_ref()
                        .ok_or_else(|| {
                            io::Error::other(
                                "Iceberg ReplacePartition requires a source namespace contract",
                            )
                        })?
                        .partition_key,
                )?),
                WritePolicy::Append | WritePolicy::ReplaceTable => None,
            }
        };
        let policy_delete_columns = match policy {
            WritePolicy::MergeByKey => resolved_contract
                .as_ref()
                .map(|contract| {
                    contract
                        .primary_key
                        .iter()
                        .map(FieldPath::dotted)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
            WritePolicy::ReplacePartition => resolved_contract
                .as_ref()
                .map(|contract| {
                    contract
                        .partition_key
                        .iter()
                        .map(FieldPath::dotted)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
            WritePolicy::Append | WritePolicy::ReplaceTable => Vec::new(),
        };

        let date_fields = Arc::new(iceberg_date_field_names(&metadata));
        let iceberg_schema = table.metadata().current_schema().clone();
        let reader_schema = reader.schema();
        let mut uploads = FuturesUnordered::new();
        let mut files = Vec::new();
        let mut transport_chunk_count = 0u32;
        let mut data_roll_index = 0u64;
        let mut delete_roll_index = 0u64;
        let mut data_rows = 0u64;
        let mut cdc_state = CdcStateSession::default();
        let mut merge_keys = HashSet::new();

        while let Some(chunk) = reader.next_chunk().await? {
            transport_chunk_count = transport_chunk_count.saturating_add(1);
            let chunk_cdc = ctx.chunk_cdc_context(&chunk)?;
            let stream = chunk.into_stream(reader_schema.clone());
            let stream = match chunk_cdc.as_ref() {
                Some(cdc) => {
                    super::cdc_encode::augment_stream_with_cdc_columns(stream, &cdc.part_meta)
                }
                None => stream,
            };
            let batches = collect_record_batches(stream).await?;
            let (data_batches, delete_batches) = if let Some(contract) = exact_cdc_contract.as_ref()
            {
                self.prepare_grouped_cdc_batches(namespace, contract, batches, &mut cdc_state)
                    .await?
            } else if matches!(
                policy,
                WritePolicy::MergeByKey | WritePolicy::ReplacePartition
            ) {
                prepare_grouped_policy_batches(
                    batches,
                    policy,
                    &policy_delete_columns,
                    &mut merge_keys,
                )?
            } else {
                (batches, Vec::new())
            };

            for roll in roll_record_batches(data_batches) {
                let identity = grouped_file_identity(
                    &manifest.idempotency_key,
                    PendingFileContent::Data,
                    data_roll_index,
                );
                data_roll_index = data_roll_index.saturating_add(1);
                uploads.push(self.upload_grouped_file(
                    namespace.to_string(),
                    PendingFileContent::Data,
                    identity,
                    roll,
                    date_fields.clone(),
                    iceberg_schema.clone(),
                    Vec::new(),
                ));
                while uploads.len() >= ICEBERG_MAX_FILE_WRITES_IN_FLIGHT {
                    let file = uploads
                        .next()
                        .await
                        .ok_or_else(|| io::Error::other("grouped upload queue closed early"))??;
                    if file.content == PendingFileContent::Data {
                        data_rows = data_rows.saturating_add(file.rows);
                    }
                    files.push(file);
                }
            }
            for roll in roll_record_batches(delete_batches) {
                let identity = grouped_file_identity(
                    &manifest.idempotency_key,
                    PendingFileContent::EqualityDeletes,
                    delete_roll_index,
                );
                delete_roll_index = delete_roll_index.saturating_add(1);
                uploads.push(self.upload_grouped_file(
                    namespace.to_string(),
                    PendingFileContent::EqualityDeletes,
                    identity,
                    roll,
                    date_fields.clone(),
                    iceberg_schema.clone(),
                    equality_ids.clone().unwrap_or_default(),
                ));
                while uploads.len() >= ICEBERG_MAX_FILE_WRITES_IN_FLIGHT {
                    let file = uploads
                        .next()
                        .await
                        .ok_or_else(|| io::Error::other("grouped upload queue closed early"))??;
                    if file.content == PendingFileContent::Data {
                        data_rows = data_rows.saturating_add(file.rows);
                    }
                    files.push(file);
                }
            }
        }
        while let Some(file) = uploads.next().await {
            let file = file?;
            if file.content == PendingFileContent::Data {
                data_rows = data_rows.saturating_add(file.rows);
            }
            files.push(file);
        }
        files.sort_by(|left, right| left.file_uri.cmp(&right.file_uri));

        Ok(IcebergGroupedPending {
            version: ICEBERG_GROUPED_PENDING_VERSION,
            manifest,
            namespace: namespace.to_string(),
            schema_version,
            transport_chunk_count,
            rows: data_rows,
            files,
            state_delta: cdc_state.delta,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn upload_grouped_file(
        &self,
        namespace: String,
        content: PendingFileContent,
        identity: String,
        batches: Vec<RecordBatch>,
        date_fields: Arc<HashSet<String>>,
        iceberg_schema: Arc<Schema>,
        equality_ids: Vec<i32>,
    ) -> Result<PendingIcebergFile, io::Error> {
        let mut prepared = Vec::with_capacity(batches.len());
        for batch in batches {
            let batch = crate::parquet_util::coerce_timestamp_dates_to_date32(batch, &date_fields)?;
            prepared.push(apply_iceberg_field_ids(batch, &iceberg_schema)?);
        }
        let schema = prepared
            .first()
            .map(RecordBatch::schema)
            .ok_or_else(|| io::Error::other("refusing to upload an empty Iceberg file"))?;
        let order_fields =
            skippr_runtime_sdk::converters::parquet_ordering::resolve_effective_order(&schema);
        let writer_properties =
            skippr_runtime_sdk::converters::parquet_ordering::build_writer_properties(
                &schema,
                &order_fields,
                skippr_runtime_sdk::converters::parquet_ordering::default_streaming_row_group_size(
                ),
            );
        let prepared = prepared
            .into_iter()
            .map(|batch| {
                skippr_runtime_sdk::converters::parquet_ordering::sort_batch(&batch, &order_fields)
                    .map_err(ObjectWriteError::input)
            })
            .collect::<Vec<_>>();
        let (bucket, object_key, file_uri) =
            self.grouped_file_location(&namespace, content, &identity)?;
        let backend = Arc::new(S3ObjectWriteBackend {
            client: self.s3_client.clone(),
            bucket,
        });
        let receipt = ObjectWriteSession::new(
            backend,
            ObjectWriteRequest::new(object_key),
            ObjectWriterConfig::default(),
        )
        .map_err(|err| io::Error::other(err.to_string()))?
        .write_parquet(schema, writer_properties, futures::stream::iter(prepared))
        .await
        .map_err(|err| io::Error::other(err.to_string()))?;
        Ok(PendingIcebergFile {
            content,
            file_uri,
            rows: receipt.rows,
            bytes: receipt.bytes,
            equality_ids,
        })
    }

    fn grouped_file_location(
        &self,
        namespace: &str,
        content: PendingFileContent,
        identity: &str,
    ) -> Result<(String, String, String), io::Error> {
        let table_location = self.table_location(namespace).ok_or_else(|| {
            io::Error::other("Iceberg sink requires table_location_prefix for data file writes")
        })?;
        let (bucket, table_prefix) = parse_s3_uri(&table_location)?;
        let content_dir = match content {
            PendingFileContent::Data => "data",
            PendingFileContent::EqualityDeletes => "delete",
        };
        let object_key = skippr_object_writer::deterministic_object_key(
            &format!("{}/{content_dir}", table_prefix.trim_matches('/')),
            identity,
            "parquet",
        )
        .map_err(|err| io::Error::other(err.to_string()))?;
        let file_uri = to_iceberg_s3_uri(&format!("s3://{bucket}/{object_key}"));
        Ok((bucket, object_key, file_uri))
    }

    async fn prepare_grouped_cdc_batches(
        &self,
        namespace: &str,
        contract: &skippr_runtime_sdk::plugins::cdc::NamespaceContract,
        batches: Vec<RecordBatch>,
        state: &mut CdcStateSession,
    ) -> Result<(Vec<RecordBatch>, Vec<RecordBatch>), io::Error> {
        if contract.business_key_columns.is_empty() {
            return Err(io::Error::other(format!(
                "Iceberg CDC namespace '{}' requires business_key_columns",
                namespace
            )));
        }
        let mut touched_shards = Vec::new();
        for batch in &batches {
            for row in 0..batch.num_rows() {
                let key = business_key_for_row(batch, &contract.business_key_columns, row)?;
                touched_shards.push(cdc_state_shard(&key));
            }
        }
        touched_shards.sort_unstable();
        touched_shards.dedup();
        self.ensure_cdc_shards_loaded(namespace, state, &touched_shards)
            .await?;
        filter_grouped_cdc_batches(contract, batches, state)
    }

    async fn ensure_cdc_shards_loaded(
        &self,
        namespace: &str,
        state: &mut CdcStateSession,
        shards: &[u8],
    ) -> Result<(), io::Error> {
        for shard in shards {
            if state.loaded_shards.contains_key(shard) {
                continue;
            }
            let values = match self.load_cdc_state_shard(namespace, *shard).await? {
                Some(state) => state,
                None => {
                    if !state.legacy_checked {
                        state.legacy_state = Some(self.load_cdc_state(namespace).await?);
                        state.legacy_checked = true;
                    }
                    let values = state
                        .legacy_state
                        .as_ref()
                        .into_iter()
                        .flat_map(|values| values.iter())
                        .filter(|(key, _)| cdc_state_shard(key) == *shard)
                        .map(|(key, token)| (key.clone(), token.clone()))
                        .collect::<BTreeMap<_, _>>();
                    LoadedCdcStateShard { values, etag: None }
                }
            };
            state.loaded_shards.insert(*shard, values);
        }
        Ok(())
    }

    async fn load_cdc_state_shard(
        &self,
        namespace: &str,
        shard: u8,
    ) -> Result<Option<LoadedCdcStateShard>, io::Error> {
        let state_uri = self.cdc_state_shard_uri(namespace, shard)?;
        let (bucket, key) = parse_s3_uri(&state_uri)?;
        let response = match self
            .s3_client
            .get_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
        {
            Ok(response) => response,
            Err(err) if is_s3_get_object_not_found_error(&err) => return Ok(None),
            Err(err) => {
                return Err(io::Error::other(format!(
                    "failed to load Iceberg CDC state shard {} from {}: {}",
                    shard, state_uri, err
                )))
            }
        };
        let etag = response.e_tag().map(str::to_string);
        let bytes = response
            .body
            .collect()
            .await
            .map_err(|err| io::Error::other(err.to_string()))?
            .into_bytes();
        let values =
            serde_json::from_slice(&bytes).map_err(|err| io::Error::other(err.to_string()))?;
        Ok(Some(LoadedCdcStateShard { values, etag }))
    }

    async fn persist_cdc_state_delta(
        &self,
        namespace: &str,
        delta: &BTreeMap<String, String>,
    ) -> Result<(), io::Error> {
        let mut by_shard = BTreeMap::<u8, Vec<(&String, &String)>>::new();
        for (key, token) in delta {
            by_shard
                .entry(cdc_state_shard(key))
                .or_default()
                .push((key, token));
        }
        let shards = by_shard.keys().copied().collect::<Vec<_>>();
        let mut state = CdcStateSession::default();
        self.ensure_cdc_shards_loaded(namespace, &mut state, &shards)
            .await?;
        for (shard, updates) in by_shard {
            let mut loaded = state.loaded_shards.remove(&shard).ok_or_else(|| {
                io::Error::other(format!(
                    "Iceberg CDC state shard {} was not loaded before persistence",
                    shard
                ))
            })?;
            let state_uri = self.cdc_state_shard_uri(namespace, shard)?;
            let (bucket, key) = parse_s3_uri(&state_uri)?;
            let mut persisted = false;
            for attempt in 1..=3 {
                if attempt > 1 {
                    loaded = match self.load_cdc_state_shard(namespace, shard).await? {
                        Some(loaded) => loaded,
                        None => LoadedCdcStateShard {
                            values: state
                                .legacy_state
                                .as_ref()
                                .into_iter()
                                .flat_map(|values| values.iter())
                                .filter(|(key, _)| cdc_state_shard(key) == shard)
                                .map(|(key, token)| (key.clone(), token.clone()))
                                .collect(),
                            etag: None,
                        },
                    };
                }
                for (business_key, token) in &updates {
                    let should_update = loaded
                        .values
                        .get(*business_key)
                        .map(|seen| {
                            compare_cdc_order_tokens(seen, token)
                                .map(|seen_is_newer| !seen_is_newer)
                        })
                        .transpose()?
                        .unwrap_or(true);
                    if should_update {
                        loaded
                            .values
                            .insert((*business_key).clone(), (*token).clone());
                    }
                }
                let bytes = serde_json::to_vec(&loaded.values)
                    .map_err(|err| io::Error::other(err.to_string()))?;
                let request = self
                    .s3_client
                    .put_object()
                    .bucket(&bucket)
                    .key(&key)
                    .body(ByteStream::from(bytes));
                let request = match loaded.etag.as_ref() {
                    Some(etag) => request.if_match(etag),
                    None => request.if_none_match("*"),
                };
                match request.send().await {
                    Ok(_) => {
                        persisted = true;
                        break;
                    }
                    Err(err)
                        if err
                            .raw_response()
                            .is_some_and(|response| response.status().as_u16() == 412)
                            && attempt < 3 =>
                    {
                        warn!(
                            "Iceberg CDC state shard {} changed concurrently; retrying conditional update",
                            shard
                        );
                    }
                    Err(err) => {
                        return Err(io::Error::other(format!(
                            "failed to persist Iceberg CDC state shard {}: {}",
                            shard, err
                        )))
                    }
                }
            }
            if !persisted {
                return Err(io::Error::other(format!(
                    "failed to persist Iceberg CDC state shard {} after conditional retries",
                    shard
                )));
            }
        }
        Ok(())
    }

    fn cdc_state_shard_uri(&self, namespace: &str, shard: u8) -> Result<String, io::Error> {
        let table_location = self.table_location(namespace).ok_or_else(|| {
            io::Error::other("Iceberg sink requires table_location_prefix for CDC state")
        })?;
        Ok(format!(
            "{}/metadata/skippr-cdc-state-v2/{shard:02}.json",
            table_location.trim_end_matches('/')
        ))
    }

    async fn native_append(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
        cdc_ctx: Option<&skippr_runtime_sdk::plugins::cdc::SyncContext>,
        object_stem: Option<&str>,
    ) -> Result<(), io::Error> {
        let namespace = BufferChunker::decode_file_namespace(&filename);
        let metadata = self.namespace_metadata(&namespace).await?;
        let catalog = self.glue_catalog().await?;
        let table = self.ensure_table(&catalog, &namespace, &metadata).await?;

        let exact_once_cdc = cdc_ctx
            .and_then(|ctx| ctx.contract.as_ref())
            .map(|contract| contract.effective_guarantee == EffectiveGuarantee::ExactOnceFinalState)
            .unwrap_or(false);
        let (data_stream, delete_stream, state_updates, upsert_rows) = if exact_once_cdc {
            let ctx = cdc_ctx.expect("exact-once CDC requires sync context");
            let prepared = self.prepare_cdc_stream(stream, &namespace, ctx).await?;
            (
                prepared.upsert_stream,
                prepared.delete_stream,
                prepared.state_updates,
                Some(prepared.upsert_rows),
            )
        } else {
            (stream, None, HashMap::new(), None)
        };

        let date_fields = iceberg_date_field_names(&metadata);
        let mut commit_files = Vec::new();
        let mut row_count = 0;
        if upsert_rows != Some(0) {
            let data_batches = collect_record_batches(data_stream).await?;
            let data_batches = apply_iceberg_field_ids_to_batches(
                data_batches,
                table.metadata().current_schema(),
            )?;
            let parquet_bytes = crate::parquet_util::serialize_to_parquet_for_iceberg(
                batch_stream(data_batches),
                &date_fields,
            )
            .await?;
            row_count = parquet_bytes.num_rows as u64;
            if row_count > 0 {
                let data_file_uri = self
                    .write_parquet_file(
                        &namespace,
                        "data",
                        &filename,
                        object_stem,
                        parquet_bytes.bytes.clone(),
                    )
                    .await?;
                commit_files.push(self.build_data_file(
                    &table,
                    DataContentType::Data,
                    data_file_uri,
                    row_count,
                    parquet_bytes.size_bytes as u64,
                    None,
                )?);
            }
        }

        if let Some(delete_stream) = delete_stream {
            let ctx = cdc_ctx
                .ok_or_else(|| io::Error::other("Iceberg CDC delete stream requires context"))?;
            let contract = ctx.contract.as_ref().ok_or_else(|| {
                io::Error::other(format!(
                    "Iceberg CDC delete stream for namespace '{}' requires a namespace contract",
                    namespace
                ))
            })?;
            let equality_ids = self
                .plan_cdc_commit(&table, &namespace, Some(ctx))
                .await?
                .ok_or_else(|| {
                    io::Error::other(format!(
                        "Iceberg CDC namespace '{}' could not plan equality-delete keys",
                        namespace
                    ))
                })?;
            let delete_batches = collect_record_batches(delete_stream).await?;
            let delete_columns = &contract.business_key_columns;
            let projected_delete_batches = delete_batches
                .iter()
                .map(|batch| project_batch_columns(batch, delete_columns))
                .collect::<Result<Vec<_>, _>>()?;
            let projected_delete_batches = apply_iceberg_field_ids_to_batches(
                projected_delete_batches,
                table.metadata().current_schema(),
            )?;
            let delete_bytes = crate::parquet_util::serialize_to_parquet_for_iceberg(
                batch_stream(projected_delete_batches),
                &date_fields,
            )
            .await?;
            let delete_row_count = delete_bytes.num_rows as u64;
            if delete_row_count > 0 {
                let delete_file_uri = self
                    .write_parquet_file(
                        &namespace,
                        "delete",
                        &filename,
                        object_stem,
                        delete_bytes.bytes,
                    )
                    .await?;
                commit_files.push(self.build_data_file(
                    &table,
                    DataContentType::EqualityDeletes,
                    delete_file_uri,
                    delete_row_count,
                    delete_bytes.size_bytes as u64,
                    Some(equality_ids),
                )?);
            }
        }

        if commit_files.is_empty() {
            info!(
                "Iceberg commit skipped empty or stale batch for namespace '{}'",
                namespace
            );
            return Ok(());
        }

        let committed = self
            .commit_append_with_retries(&catalog, table.identifier().clone(), commit_files)
            .await?;
        if !state_updates.is_empty() {
            self.persist_cdc_state(&namespace, state_updates).await?;
        }
        info!(
            "Committed Iceberg append namespace={} table={} rows={}",
            namespace,
            committed.identifier(),
            row_count
        );
        Ok(())
    }

    async fn native_policy_write(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
        contract: &SourceNamespaceContract,
        policy: WritePolicy,
        object_stem: Option<&str>,
    ) -> Result<(), io::Error> {
        use iceberg::Catalog;

        let namespace = BufferChunker::decode_file_namespace(&filename);
        let metadata = self.namespace_metadata(&namespace).await?;
        let catalog = self.glue_catalog().await?;

        if policy == WritePolicy::ReplaceTable {
            let table_ident = self.table_ident(&namespace)?;
            if catalog
                .table_exists(&table_ident)
                .await
                .map_err(|err| io::Error::other(err.to_string()))?
            {
                catalog
                    .drop_table(&table_ident)
                    .await
                    .map_err(|err| io::Error::other(err.to_string()))?;
                info!(
                    "Iceberg ReplaceTable dropped existing table '{}' before recreate",
                    table_ident
                );
            }
            self.ensure_table(&catalog, &namespace, &metadata).await?;
            return self
                .native_append(stream, filename, None, object_stem)
                .await;
        }

        let table = self.ensure_table(&catalog, &namespace, &metadata).await?;
        let batches = collect_record_batches(stream).await?;
        if batches.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Iceberg {:?} for namespace '{}' requires at least one row to derive delete scope",
                    policy, namespace
                ),
            ));
        }

        let schema = batches[0].schema();
        let data_batch = arrow::compute::concat_batches(&schema, &batches)
            .map_err(|err| io::Error::other(err.to_string()))?;

        let delete_paths = match policy {
            WritePolicy::MergeByKey => &contract.primary_key,
            WritePolicy::ReplacePartition => &contract.partition_key,
            WritePolicy::Append | WritePolicy::ReplaceTable => {
                return Err(io::Error::other("unexpected write policy branch"));
            }
        };
        if delete_paths.is_empty() {
            return Err(io::Error::other(format!(
                "Iceberg {:?} for namespace '{}' requires non-empty key columns in source contract",
                policy, namespace
            )));
        }
        validate_flat_field_paths(delete_paths)?;

        let delete_columns: Vec<String> = delete_paths.iter().map(FieldPath::dotted).collect();
        if matches!(policy, WritePolicy::MergeByKey) {
            validate_no_duplicate_batch_keys(&data_batch, &delete_columns)?;
        }
        let delete_batch = project_batch_columns(
            &dedupe_batch_by_columns(&data_batch, &delete_columns)?,
            &delete_columns,
        )?;
        let equality_ids = plan_contract_equality_ids(&table, delete_paths)?;

        let date_fields = iceberg_date_field_names(&metadata);
        let mut commit_files = Vec::new();
        let mut row_count = 0u64;

        if delete_batch.num_rows() > 0 {
            let delete_batch =
                apply_iceberg_field_ids(delete_batch, table.metadata().current_schema())?;
            let delete_stream = batch_stream(vec![delete_batch]);
            let delete_bytes =
                crate::parquet_util::serialize_to_parquet_for_iceberg(delete_stream, &date_fields)
                    .await?;
            let delete_row_count = delete_bytes.num_rows as u64;
            if delete_row_count > 0 {
                let delete_file_uri = self
                    .write_parquet_file(
                        &namespace,
                        "delete",
                        &filename,
                        object_stem,
                        delete_bytes.bytes,
                    )
                    .await?;
                commit_files.push(self.build_data_file(
                    &table,
                    DataContentType::EqualityDeletes,
                    delete_file_uri,
                    delete_row_count,
                    delete_bytes.size_bytes as u64,
                    Some(equality_ids.clone()),
                )?);
            }
        }

        if data_batch.num_rows() > 0 {
            let data_batch =
                apply_iceberg_field_ids(data_batch, table.metadata().current_schema())?;
            let data_stream = batch_stream(vec![data_batch]);
            let parquet_bytes =
                crate::parquet_util::serialize_to_parquet_for_iceberg(data_stream, &date_fields)
                    .await?;
            row_count = parquet_bytes.num_rows as u64;
            if row_count > 0 {
                let data_file_uri = self
                    .write_parquet_file(
                        &namespace,
                        "data",
                        &filename,
                        object_stem,
                        parquet_bytes.bytes.clone(),
                    )
                    .await?;
                commit_files.push(self.build_data_file(
                    &table,
                    DataContentType::Data,
                    data_file_uri,
                    row_count,
                    parquet_bytes.size_bytes as u64,
                    None,
                )?);
            }
        }

        if commit_files.is_empty() {
            return Ok(());
        }

        let committed = self
            .commit_append_with_retries(&catalog, table.identifier().clone(), commit_files)
            .await?;
        info!(
            "Committed Iceberg {:?} namespace={} table={} rows={}",
            policy,
            namespace,
            committed.identifier(),
            row_count
        );
        Ok(())
    }

    fn build_data_file(
        &self,
        table: &iceberg::table::Table,
        content_type: DataContentType,
        file_uri: String,
        row_count: u64,
        size_bytes: u64,
        equality_ids: Option<Vec<i32>>,
    ) -> Result<iceberg::spec::DataFile, io::Error> {
        let mut builder = DataFileBuilder::default();
        builder
            .content(content_type)
            .file_path(file_uri)
            .file_format(DataFileFormat::Parquet)
            .partition(Struct::empty())
            .record_count(row_count)
            .file_size_in_bytes(size_bytes)
            .partition_spec_id(table.metadata().default_partition_spec().spec_id());
        if equality_ids.is_some() {
            builder.equality_ids(equality_ids);
        }
        builder
            .build()
            .map_err(|err| io::Error::other(err.to_string()))
    }

    async fn commit_append_with_retries(
        &self,
        catalog: &GlueCatalog,
        table_ident: TableIdent,
        commit_files: Vec<iceberg::spec::DataFile>,
    ) -> Result<iceberg::table::Table, io::Error> {
        let (data_files, delete_files) = partition_commit_files(commit_files);

        if delete_files.is_empty() {
            return self
                .commit_data_append_with_retries(catalog, table_ident, data_files)
                .await;
        }

        self.commit_equality_delta_with_retries(catalog, table_ident, data_files, delete_files)
            .await
    }

    async fn commit_grouped_append_with_retries(
        &self,
        catalog: &GlueCatalog,
        table_ident: TableIdent,
        commit_files: Vec<iceberg::spec::DataFile>,
        manifest: &ObjectWriteManifest,
    ) -> Result<GroupedCommitResult, io::Error> {
        let (data_files, delete_files) = partition_commit_files(commit_files);
        if delete_files.is_empty() {
            return self
                .commit_grouped_data_append_with_retries(catalog, table_ident, data_files, manifest)
                .await;
        }
        self.commit_grouped_equality_delta_with_retries(
            catalog,
            table_ident,
            data_files,
            delete_files,
            manifest,
        )
        .await
    }

    async fn commit_grouped_data_append_with_retries(
        &self,
        catalog: &GlueCatalog,
        table_ident: TableIdent,
        data_files: Vec<iceberg::spec::DataFile>,
        manifest: &ObjectWriteManifest,
    ) -> Result<GroupedCommitResult, io::Error> {
        let mut last_err = None;
        for attempt in 1..=3 {
            let table = catalog
                .load_table(&table_ident)
                .await
                .map_err(|err| io::Error::other(err.to_string()))?;
            if table_has_grouped_snapshot(&table, manifest) {
                return Ok(GroupedCommitResult {
                    table,
                    already_committed: true,
                });
            }
            let tx = Transaction::new(&table);
            let tx = tx
                .fast_append()
                .set_snapshot_properties(grouped_snapshot_properties(manifest))
                .add_data_files(data_files.clone())
                .apply(tx)
                .map_err(|err| io::Error::other(err.to_string()))?;
            match tx.commit(catalog).await {
                Ok(table) => {
                    return Ok(GroupedCommitResult {
                        table,
                        already_committed: false,
                    })
                }
                Err(err) => {
                    let message = err.to_string();
                    if is_duplicate_iceberg_file_error(&message) {
                        let table = catalog
                            .load_table(&table_ident)
                            .await
                            .map_err(|err| io::Error::other(err.to_string()))?;
                        return Ok(GroupedCommitResult {
                            table,
                            already_committed: true,
                        });
                    }
                    warn!(
                        "Iceberg grouped append commit attempt {} failed for {}: {}",
                        attempt, table_ident, message
                    );
                    last_err = Some(message);
                }
            }
        }
        Err(io::Error::other(format!(
            "Iceberg grouped append commit failed after retries for {}: {}",
            table_ident,
            last_err.unwrap_or_else(|| "unknown error".to_string())
        )))
    }

    async fn commit_grouped_equality_delta_with_retries(
        &self,
        catalog: &GlueCatalog,
        table_ident: TableIdent,
        data_files: Vec<iceberg::spec::DataFile>,
        delete_files: Vec<iceberg::spec::DataFile>,
        manifest: &ObjectWriteManifest,
    ) -> Result<GroupedCommitResult, io::Error> {
        let mut last_err = None;
        for attempt in 1..=3 {
            let table = catalog
                .load_table(&table_ident)
                .await
                .map_err(|err| io::Error::other(err.to_string()))?;
            if table_has_grouped_snapshot(&table, manifest) {
                return Ok(GroupedCommitResult {
                    table,
                    already_committed: true,
                });
            }
            let tx = Transaction::new(&table);
            let mut action = tx
                .equality_delta_append()
                .set_snapshot_properties(grouped_snapshot_properties(manifest))
                .add_delete_files(delete_files.clone());
            if !data_files.is_empty() {
                action = action.add_data_files(data_files.clone());
            }
            let tx = action
                .apply(tx)
                .map_err(|err| io::Error::other(err.to_string()))?;
            match tx.commit(catalog).await {
                Ok(table) => {
                    return Ok(GroupedCommitResult {
                        table,
                        already_committed: false,
                    })
                }
                Err(err) => {
                    let message = err.to_string();
                    if is_duplicate_iceberg_file_error(&message) {
                        let table = catalog
                            .load_table(&table_ident)
                            .await
                            .map_err(|err| io::Error::other(err.to_string()))?;
                        return Ok(GroupedCommitResult {
                            table,
                            already_committed: true,
                        });
                    }
                    warn!(
                        "Iceberg grouped equality-delta commit attempt {} failed for {}: {}",
                        attempt, table_ident, message
                    );
                    last_err = Some(message);
                }
            }
        }
        Err(io::Error::other(format!(
            "Iceberg grouped equality-delta commit failed after retries for {}: {}",
            table_ident,
            last_err.unwrap_or_else(|| "unknown error".to_string())
        )))
    }

    async fn commit_data_append_with_retries(
        &self,
        catalog: &GlueCatalog,
        table_ident: TableIdent,
        data_files: Vec<iceberg::spec::DataFile>,
    ) -> Result<iceberg::table::Table, io::Error> {
        let mut last_err: Option<String> = None;
        for attempt in 1..=3 {
            let table = catalog
                .load_table(&table_ident)
                .await
                .map_err(|err| io::Error::other(err.to_string()))?;
            let tx = Transaction::new(&table);
            let tx = tx
                .fast_append()
                .add_data_files(data_files.clone())
                .apply(tx)
                .map_err(|err| io::Error::other(err.to_string()))?;
            match tx.commit(catalog).await {
                Ok(table) => return Ok(table),
                Err(err) => {
                    let err = err.to_string();
                    if is_duplicate_iceberg_file_error(&err) {
                        info!(
                            "Iceberg append commit for {} already applied (idempotent WAL replay)",
                            table_ident
                        );
                        return catalog
                            .load_table(&table_ident)
                            .await
                            .map_err(|err| io::Error::other(err.to_string()));
                    }
                    warn!(
                        "Iceberg append commit attempt {} failed for {}: {}",
                        attempt, table_ident, err
                    );
                    last_err = Some(err);
                }
            }
        }
        Err(io::Error::other(format!(
            "Iceberg append commit failed after retries for {}: {}",
            table_ident,
            last_err.unwrap_or_else(|| "unknown error".to_string())
        )))
    }

    async fn commit_equality_delta_with_retries(
        &self,
        catalog: &GlueCatalog,
        table_ident: TableIdent,
        data_files: Vec<iceberg::spec::DataFile>,
        delete_files: Vec<iceberg::spec::DataFile>,
    ) -> Result<iceberg::table::Table, io::Error> {
        let mut last_err: Option<String> = None;
        for attempt in 1..=3 {
            let table = catalog
                .load_table(&table_ident)
                .await
                .map_err(|err| io::Error::other(err.to_string()))?;
            let tx = Transaction::new(&table);
            let mut action = tx
                .equality_delta_append()
                .add_delete_files(delete_files.clone());
            if !data_files.is_empty() {
                action = action.add_data_files(data_files.clone());
            }
            let tx = action
                .apply(tx)
                .map_err(|err| io::Error::other(err.to_string()))?;
            match tx.commit(catalog).await {
                Ok(table) => return Ok(table),
                Err(err) => {
                    let err = err.to_string();
                    if is_duplicate_iceberg_file_error(&err) {
                        info!(
                            "Iceberg equality-delta commit for {} already applied (idempotent WAL replay)",
                            table_ident
                        );
                        return catalog
                            .load_table(&table_ident)
                            .await
                            .map_err(|err| io::Error::other(err.to_string()));
                    }
                    warn!(
                        "Iceberg equality-delta commit attempt {} failed for {}: {}",
                        attempt, table_ident, err
                    );
                    last_err = Some(err);
                }
            }
        }
        Err(io::Error::other(format!(
            "Iceberg equality-delta commit failed after retries for {}: {}",
            table_ident,
            last_err.unwrap_or_else(|| "unknown error".to_string())
        )))
    }

    async fn prepare_cdc_stream(
        &self,
        mut stream: SendableRecordBatchStream,
        namespace: &str,
        ctx: &skippr_runtime_sdk::plugins::cdc::SyncContext,
    ) -> Result<PreparedCdcStreams, io::Error> {
        let contract = ctx.contract.as_ref().ok_or_else(|| {
            io::Error::other(format!(
                "Iceberg CDC namespace '{}' requires a resolved CDC contract",
                namespace
            ))
        })?;
        let mut state = self.load_cdc_state(namespace).await?;
        let mut upsert_batches = Vec::new();
        let mut delete_batches = Vec::new();
        let mut state_updates = HashMap::new();
        let mut upsert_rows = 0u64;

        while let Some(batch) = stream.next().await {
            let batch = batch.map_err(|err| io::Error::other(err.to_string()))?;
            let mutation_idx = batch
                .schema()
                .index_of("_skippr_mutation")
                .map_err(|err| io::Error::other(err.to_string()))?;
            let order_idx = batch
                .schema()
                .index_of("_skippr_order_token")
                .map_err(|err| io::Error::other(err.to_string()))?;
            let mutations = batch
                .column(mutation_idx)
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| io::Error::other("_skippr_mutation must be Utf8"))?;
            let orders = batch
                .column(order_idx)
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| io::Error::other("_skippr_order_token must be Utf8"))?;

            let mut upsert_mask = Vec::with_capacity(batch.num_rows());
            let mut delete_mask = Vec::with_capacity(batch.num_rows());

            for row in 0..batch.num_rows() {
                let key = business_key_for_row(&batch, &contract.business_key_columns, row)?;
                let order_token = validate_cdc_order_token(orders.value(row))?.to_string();
                let is_stale = state
                    .get(&key)
                    .map(|seen| compare_cdc_order_tokens(seen, &order_token))
                    .transpose()?
                    .unwrap_or(false);
                if is_stale {
                    upsert_mask.push(false);
                    delete_mask.push(false);
                    continue;
                }

                state.insert(key.clone(), order_token.clone());
                state_updates.insert(key, order_token);
                match mutations.value(row) {
                    "delete" => {
                        upsert_mask.push(false);
                        delete_mask.push(true);
                    }
                    "snapshot" | "insert" | "update" => {
                        upsert_mask.push(true);
                        delete_mask.push(false);
                    }
                    other => {
                        return Err(io::Error::other(format!(
                            "unsupported CDC mutation '{}'",
                            other
                        )));
                    }
                }
            }

            let upsert_filter = BooleanArray::from(upsert_mask);
            let delete_filter = BooleanArray::from(delete_mask);
            let upsert = filter_record_batch(&batch, &upsert_filter)
                .map_err(|err| io::Error::other(err.to_string()))?;
            let delete = filter_record_batch(&batch, &delete_filter)
                .map_err(|err| io::Error::other(err.to_string()))?;
            if upsert.num_rows() > 0 {
                upsert_rows += upsert.num_rows() as u64;
                upsert_batches.push(upsert);
            }
            if delete.num_rows() > 0 {
                delete_batches.push(project_batch_columns(
                    &delete,
                    &contract.business_key_columns,
                )?);
            }
        }

        let upsert_stream = batch_stream(upsert_batches);
        let delete_stream = if delete_batches.is_empty() {
            None
        } else {
            Some(batch_stream(delete_batches))
        };
        Ok(PreparedCdcStreams {
            upsert_stream,
            delete_stream,
            state_updates,
            upsert_rows,
        })
    }

    async fn load_cdc_state(&self, namespace: &str) -> Result<HashMap<String, String>, io::Error> {
        let state_uri = self.cdc_state_uri(namespace)?;
        let (bucket, key) = parse_s3_uri(&state_uri)?;
        match self
            .s3_client
            .get_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
        {
            Ok(output) => {
                let bytes = output
                    .body
                    .collect()
                    .await
                    .map_err(|err| io::Error::other(err.to_string()))?
                    .into_bytes();
                serde_json::from_slice(&bytes).map_err(|err| io::Error::other(err.to_string()))
            }
            Err(err) => {
                if is_s3_get_object_not_found_error(&err) {
                    Ok(HashMap::new())
                } else {
                    let err_text = err.to_string();
                    Err(io::Error::other(format!(
                        "failed to load Iceberg CDC state from {}: {}",
                        state_uri, err_text
                    )))
                }
            }
        }
    }

    async fn persist_cdc_state(
        &self,
        namespace: &str,
        updates: HashMap<String, String>,
    ) -> Result<(), io::Error> {
        let mut state = self.load_cdc_state(namespace).await?;
        for (key, token) in updates {
            let should_update = state
                .get(&key)
                .map(|seen| {
                    compare_cdc_order_tokens(seen, &token).map(|is_seen_newer| !is_seen_newer)
                })
                .transpose()?
                .unwrap_or(true);
            if should_update {
                state.insert(key, token);
            }
        }
        let state_uri = self.cdc_state_uri(namespace)?;
        let (bucket, key) = parse_s3_uri(&state_uri)?;
        let bytes = serde_json::to_vec(&state).map_err(|err| io::Error::other(err.to_string()))?;
        self.s3_client
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(ByteStream::from(bytes))
            .send()
            .await
            .map_err(|err| io::Error::other(err.to_string()))?;
        Ok(())
    }

    fn cdc_state_uri(&self, namespace: &str) -> Result<String, io::Error> {
        let table_location = self.table_location(namespace).ok_or_else(|| {
            io::Error::other("Iceberg sink requires table_location_prefix for CDC state")
        })?;
        Ok(format!(
            "{}/metadata/skippr-cdc-state.json",
            table_location.trim_end_matches('/')
        ))
    }

    async fn write_parquet_file(
        &self,
        namespace: &str,
        content_dir: &str,
        filename: &str,
        object_stem: Option<&str>,
        bytes: bytes::Bytes,
    ) -> Result<String, io::Error> {
        let table_location = self.table_location(namespace).ok_or_else(|| {
            io::Error::other("Iceberg sink requires table_location_prefix for data file writes")
        })?;
        let (bucket, table_prefix) = parse_s3_uri(&table_location)?;
        let digest = object_stem
            .map(str::to_string)
            .unwrap_or_else(|| hex::encode(md5::compute(filename).0));
        let key = format!(
            "{}/{}/{}.parquet",
            table_prefix.trim_matches('/'),
            content_dir,
            digest
        );
        self.s3_client
            .put_object()
            .bucket(&bucket)
            .key(&key)
            .body(ByteStream::from(bytes))
            .send()
            .await
            .map_err(|err| {
                io::Error::other(format!("Failed to upload Iceberg data file: {err}"))
            })?;
        Ok(to_iceberg_s3_uri(&format!("s3://{}/{}", bucket, key)))
    }

    fn idempotency_manifest_location(
        &self,
        namespace: &str,
        idempotency_key: &str,
    ) -> Result<(String, String), io::Error> {
        let table_location = self.table_location(namespace).ok_or_else(|| {
            io::Error::other(
                "Iceberg sink requires table_location_prefix for idempotency manifests",
            )
        })?;
        let (bucket, table_prefix) = parse_s3_uri(&table_location)?;
        let object_name = manifest_object_name(&format!("{idempotency_key}.json"));
        Ok((
            bucket,
            format!(
                "{}/metadata/skippr-idempotency/{}",
                table_prefix.trim_matches('/'),
                object_name
            ),
        ))
    }

    async fn read_manifest(
        &self,
        bucket: &str,
        key: &str,
    ) -> io::Result<Option<ObjectWriteManifest>> {
        let response = match self
            .s3_client
            .get_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
        {
            Ok(response) => response,
            Err(err) if is_s3_get_object_not_found_error(&err) => return Ok(None),
            Err(err) => {
                return Err(io::Error::other(format!(
                    "Failed to read Iceberg idempotency manifest s3://{}/{}: {}",
                    bucket, key, err
                )))
            }
        };
        let bytes = response
            .body
            .collect()
            .await
            .map_err(|err| io::Error::other(err.to_string()))?
            .into_bytes();
        ObjectWriteManifest::from_json_bytes(&bytes).map(Some)
    }

    async fn manifest_matches(
        &self,
        bucket: &str,
        key: &str,
        expected: &ObjectWriteManifest,
    ) -> io::Result<bool> {
        let response = match self
            .s3_client
            .get_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
        {
            Ok(response) => response,
            Err(err) => {
                if is_s3_get_object_not_found_error(&err) {
                    return Ok(false);
                }
                return Err(io::Error::other(format!(
                    "Failed to read Iceberg idempotency manifest s3://{}/{}: {}",
                    bucket, key, err
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
        bucket: &str,
        key: &str,
        manifest: &ObjectWriteManifest,
    ) -> io::Result<()> {
        self.s3_client
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(ByteStream::from(manifest.to_json_bytes()?))
            .send()
            .await
            .map_err(|err| {
                io::Error::other(format!(
                    "Failed to write Iceberg idempotency manifest s3://{}/{}: {}",
                    bucket, key, err
                ))
            })?;
        Ok(())
    }

    async fn plan_cdc_commit(
        &self,
        table: &iceberg::table::Table,
        namespace: &str,
        cdc_ctx: Option<&skippr_runtime_sdk::plugins::cdc::SyncContext>,
    ) -> Result<Option<Vec<i32>>, io::Error> {
        let Some(ctx) = cdc_ctx else {
            return Ok(None);
        };
        let Some(contract) = ctx.contract.as_ref() else {
            warn!(
                "Iceberg received CDC rows for namespace '{}' without a namespace contract; committing encoded CDC rows",
                namespace
            );
            return Ok(None);
        };
        if contract.business_key_columns.is_empty() {
            return Err(io::Error::other(format!(
                "Iceberg CDC namespace '{}' requires business_key_columns",
                namespace
            )));
        }
        let schema = table.metadata().current_schema();
        let mut equality_ids = Vec::with_capacity(contract.business_key_columns.len());
        for column in &contract.business_key_columns {
            let field = schema.field_by_name(column).ok_or_else(|| {
                io::Error::other(format!(
                    "Iceberg CDC business key '{}' is not present in namespace '{}'",
                    column, namespace
                ))
            })?;
            equality_ids.push(field.id);
        }
        Ok(Some(equality_ids))
    }

    async fn namespace_metadata(&self, namespace: &str) -> Result<OutputMetadata, io::Error> {
        let guard = self.schema_state.read().await;
        guard.namespaces.get(namespace).cloned().ok_or_else(|| {
            io::Error::other(format!(
                "Iceberg sink schema state v{} is missing namespace '{}'",
                guard.version, namespace
            ))
        })
    }

    async fn glue_catalog(&self) -> Result<Arc<GlueCatalog>, io::Error> {
        if let Some(catalog) = self.catalog_cache.read().await.clone() {
            return Ok(catalog);
        }
        let IcebergCatalogConfig::Glue {
            warehouse,
            catalog_id,
            region,
            ..
        } = &self.config.catalog
        else {
            return Err(io::Error::other(format!(
                "Iceberg catalog adapter '{}' is configured but not implemented yet",
                self.config.catalog.adapter_name()
            )));
        };

        let mut props = HashMap::from([(
            GLUE_CATALOG_PROP_WAREHOUSE.to_string(),
            to_iceberg_s3_uri(warehouse),
        )]);
        if let Some(catalog_id) = catalog_id {
            props.insert(GLUE_CATALOG_PROP_CATALOG_ID.to_string(), catalog_id.clone());
        }
        if let Some(region) = region {
            props.insert(AWS_REGION_NAME.to_string(), region.clone());
        }
        let catalog = Arc::new(
            GlueCatalogBuilder::default()
                .load("glue", props)
                .await
                .map_err(|err| io::Error::other(err.to_string()))?,
        );
        let mut cache = self.catalog_cache.write().await;
        Ok(cache.get_or_insert_with(|| catalog.clone()).clone())
    }

    async fn ensure_table(
        &self,
        catalog: &GlueCatalog,
        namespace: &str,
        metadata: &OutputMetadata,
    ) -> Result<iceberg::table::Table, io::Error> {
        let table_ident = self.table_ident(namespace)?;
        if catalog
            .table_exists(&table_ident)
            .await
            .map_err(|err| io::Error::other(err.to_string()))?
        {
            let table = catalog
                .load_table(&table_ident)
                .await
                .map_err(|err| io::Error::other(err.to_string()))?;
            let desired_schema = iceberg_schema_from_output_metadata(namespace, metadata)?;
            if iceberg_schema_has_all_fields(table.metadata().current_schema(), &desired_schema) {
                return Ok(table);
            }

            info!(
                "Evolving Iceberg schema namespace={} table={} current_fields={} desired_fields={}",
                namespace,
                table_ident,
                table.metadata().current_schema().as_struct().fields().len(),
                desired_schema.as_struct().fields().len()
            );
            let evolved_schema = merge_iceberg_schema_missing_fields(
                namespace,
                table.metadata().current_schema(),
                &desired_schema,
            )?;
            let tx = Transaction::new(&table);
            let tx = tx
                .replace_schema(evolved_schema)
                .apply(tx)
                .map_err(|err| io::Error::other(err.to_string()))?;
            let tx = tx
                .remove_old_schemas()
                .apply(tx)
                .map_err(|err| io::Error::other(err.to_string()))?;
            return tx
                .commit(catalog)
                .await
                .map_err(|err| io::Error::other(err.to_string()));
        }

        let catalog_namespace = self.catalog_namespace()?;
        let namespace_ident = NamespaceIdent::from_strs([catalog_namespace.as_str()])
            .map_err(|err| io::Error::other(err.to_string()))?;
        self.ensure_catalog_namespace(catalog, &namespace_ident)
            .await?;

        let iceberg_schema = iceberg_schema_from_output_metadata(namespace, metadata)?;
        let mut properties: HashMap<String, String> = self
            .config
            .properties
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        properties.insert(
            "skippr.pipeline".to_string(),
            self.context.pipeline_name.clone(),
        );
        properties.insert("skippr.binding".to_string(), format!("{:?}", self.binding));

        let creation = TableCreation::builder()
            .name(table_ident.name().to_string())
            .schema(iceberg_schema)
            .properties(properties)
            .build();
        let creation = if let Some(location) = self.table_location(namespace) {
            TableCreation {
                location: Some(location),
                ..creation
            }
        } else {
            creation
        };

        match catalog.create_table(&namespace_ident, creation).await {
            Ok(table) => Ok(table),
            Err(err) => {
                let message = err.to_string();
                if message.contains("AlreadyExistsException")
                    || message.contains("Table already exists")
                {
                    return catalog
                        .load_table(&table_ident)
                        .await
                        .map_err(|err| io::Error::other(err.to_string()));
                }
                Err(io::Error::other(message))
            }
        }
    }

    fn catalog_namespace(&self) -> Result<String, io::Error> {
        if let Some(namespace) = &self.config.table_namespace {
            return Ok(namespace.clone());
        }
        match &self.config.catalog {
            IcebergCatalogConfig::Glue { database, .. } => database
                .clone()
                .ok_or_else(|| io::Error::other("Iceberg Glue catalog requires database or table_namespace")),
            IcebergCatalogConfig::Rest { .. }
            | IcebergCatalogConfig::Unity { .. }
            | IcebergCatalogConfig::Polaris { .. } => Err(io::Error::other(format!(
                "Iceberg catalog adapter '{}' requires table_namespace until its catalog-specific namespace discovery is implemented",
                self.config.catalog.adapter_name()
            ))),
        }
    }

    async fn ensure_catalog_namespace(
        &self,
        catalog: &GlueCatalog,
        namespace_ident: &NamespaceIdent,
    ) -> Result<(), io::Error> {
        for attempt in 1..=3 {
            if catalog
                .namespace_exists(namespace_ident)
                .await
                .map_err(|err| io::Error::other(err.to_string()))?
            {
                return Ok(());
            }

            match catalog
                .create_namespace(namespace_ident, HashMap::new())
                .await
            {
                Ok(_) => return Ok(()),
                Err(err) => {
                    let message = err.to_string();
                    if message.contains("AlreadyExistsException") {
                        return Ok(());
                    }
                    if message.contains("ConcurrentModificationException") && attempt < 3 {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                    return Err(io::Error::other(message));
                }
            }
        }

        Err(io::Error::other(
            "Iceberg Glue namespace creation did not converge after retries",
        ))
    }

    fn table_ident(&self, namespace: &str) -> Result<TableIdent, io::Error> {
        let catalog_namespace = self.catalog_namespace()?;
        let table_name = self.table_name(namespace);
        TableIdent::from_strs([catalog_namespace.as_str(), table_name.as_str()])
            .map_err(|err| io::Error::other(err.to_string()))
    }

    fn table_name(&self, namespace: &str) -> String {
        let namespace_name = iceberg_table_suffix(namespace);
        match &self.config.table_prefix {
            Some(prefix) if !prefix.is_empty() => format!("{}_{}", prefix, namespace_name),
            _ => namespace_name,
        }
    }

    fn table_location(&self, namespace: &str) -> Option<String> {
        self.config.table_location_prefix.as_ref().map(|prefix| {
            to_iceberg_s3_uri(&format!(
                "{}/{}",
                prefix.trim_end_matches('/'),
                self.table_name(namespace)
            ))
        })
    }
}

fn grouped_pending_key(manifest_key: &str) -> String {
    format!("{manifest_key}.pending-v2.json")
}

fn schema_version_invalidates_table_cache(installed: u64, incoming: u64) -> bool {
    incoming > installed
}

fn grouped_preparation_requires_lane(
    ctx: &skippr_runtime_sdk::plugins::GroupedSinkWriteContext<'_>,
    namespace: &str,
) -> bool {
    if ctx
        .cdc_ctx
        .and_then(|cdc| cdc.contract.as_ref())
        .is_some_and(|contract| {
            contract.effective_guarantee == EffectiveGuarantee::ExactOnceFinalState
        })
    {
        return true;
    }
    if ctx.cdc_ctx.is_some() {
        return false;
    }
    ctx.source_contract
        .cloned()
        .or_else(|| namespace_source_contract(namespace))
        .is_some_and(|contract| contract.write_policy == WritePolicy::ReplaceTable)
}

fn validate_grouped_pending(
    pending: &IcebergGroupedPending,
    namespace: &str,
    manifest: &ObjectWriteManifest,
    compaction_id: &str,
) -> Result<(), io::Error> {
    if pending.version != ICEBERG_GROUPED_PENDING_VERSION || pending.namespace != namespace {
        return Err(io::Error::other(format!(
            "unsupported Iceberg grouped pending receipt v{} for namespace '{}'",
            pending.version, pending.namespace
        )));
    }
    if !pending.manifest.matches_manifest(manifest) {
        return Err(io::Error::other(format!(
            "Iceberg grouped pending receipt mismatch for compaction '{}'",
            compaction_id
        )));
    }
    Ok(())
}

fn legacy_grouped_object_keys(table_prefix: &str, idempotency_key: &str) -> Vec<String> {
    [
        idempotency_key.to_string(),
        format!("{idempotency_key}-chunk-00000000"),
    ]
    .into_iter()
    .flat_map(|identity| {
        ["data", "delete"].into_iter().map(move |content_dir| {
            format!(
                "{}/{}/{}.parquet",
                table_prefix.trim_matches('/'),
                content_dir,
                identity
            )
        })
    })
    .collect()
}

fn grouped_file_identity(
    idempotency_key: &str,
    content: PendingFileContent,
    roll_index: u64,
) -> String {
    let content = match content {
        PendingFileContent::Data => "data",
        PendingFileContent::EqualityDeletes => "delete",
    };
    format!("{idempotency_key}-{content}-{roll_index:08}")
}

fn cdc_state_shard(business_key: &str) -> u8 {
    md5::compute(business_key.as_bytes()).0[0] % ICEBERG_CDC_STATE_SHARDS
}

fn record_batch_memory_size(batch: &RecordBatch) -> usize {
    batch
        .columns()
        .iter()
        .map(|column| column.get_array_memory_size())
        .sum()
}

fn roll_record_batches(batches: Vec<RecordBatch>) -> Vec<Vec<RecordBatch>> {
    let mut rolls = Vec::new();
    let mut current = Vec::new();
    let mut current_rows = 0usize;
    let mut current_bytes = 0usize;
    for batch in batches {
        if batch.num_rows() == 0 {
            continue;
        }
        let batch_bytes = record_batch_memory_size(&batch);
        let bytes_per_row =
            batch_bytes.saturating_add(batch.num_rows().saturating_sub(1)) / batch.num_rows();
        let rows_by_bytes = if bytes_per_row == 0 {
            ICEBERG_FILE_MAX_ROWS
        } else {
            (ICEBERG_FILE_MAX_INPUT_BYTES / bytes_per_row).max(1)
        };
        let slice_rows = ICEBERG_FILE_MAX_ROWS.min(rows_by_bytes).max(1);
        let mut offset = 0usize;
        while offset < batch.num_rows() {
            let length = slice_rows.min(batch.num_rows() - offset);
            let slice = batch.slice(offset, length);
            let slice_bytes = record_batch_memory_size(&slice);
            let would_exceed = !current.is_empty()
                && (current_rows.saturating_add(length) > ICEBERG_FILE_MAX_ROWS
                    || current_bytes.saturating_add(slice_bytes) > ICEBERG_FILE_MAX_INPUT_BYTES);
            if would_exceed {
                rolls.push(std::mem::take(&mut current));
                current_rows = 0;
                current_bytes = 0;
            }
            current_rows = current_rows.saturating_add(length);
            current_bytes = current_bytes.saturating_add(slice_bytes);
            current.push(slice);
            offset = offset.saturating_add(length);
        }
    }
    if !current.is_empty() {
        rolls.push(current);
    }
    rolls
}

fn prepare_grouped_policy_batches(
    batches: Vec<RecordBatch>,
    policy: WritePolicy,
    delete_columns: &[String],
    merge_keys: &mut HashSet<String>,
) -> Result<(Vec<RecordBatch>, Vec<RecordBatch>), io::Error> {
    let mut delete_batches = Vec::new();
    for batch in &batches {
        if policy == WritePolicy::MergeByKey {
            for row in 0..batch.num_rows() {
                let key = business_key_for_row(batch, delete_columns, row)?;
                if !merge_keys.contains(&key) && merge_keys.len() >= ICEBERG_MAX_ENVELOPE_KEYS {
                    return Err(io::Error::other(format!(
                        "Iceberg grouped MergeByKey envelope exceeds the {} unique-key session bound",
                        ICEBERG_MAX_ENVELOPE_KEYS
                    )));
                }
                if !merge_keys.insert(key) {
                    return Err(io::Error::other(format!(
                        "Iceberg MergeByKey grouped envelope contains a duplicate key at row {}; provide a deterministic source order or deduplicate upstream",
                        row
                    )));
                }
            }
        }
        let delete_batch = project_batch_columns(
            &dedupe_batch_by_columns(batch, delete_columns)?,
            delete_columns,
        )?;
        if delete_batch.num_rows() > 0 {
            delete_batches.push(delete_batch);
        }
    }
    Ok((batches, delete_batches))
}

fn filter_grouped_cdc_batches(
    contract: &skippr_runtime_sdk::plugins::cdc::NamespaceContract,
    batches: Vec<RecordBatch>,
    state: &mut CdcStateSession,
) -> Result<(Vec<RecordBatch>, Vec<RecordBatch>), io::Error> {
    let mut upsert_batches = Vec::new();
    let mut delete_batches = Vec::new();
    for batch in batches {
        let mutation_idx = batch
            .schema()
            .index_of("_skippr_mutation")
            .map_err(|err| io::Error::other(err.to_string()))?;
        let order_idx = batch
            .schema()
            .index_of("_skippr_order_token")
            .map_err(|err| io::Error::other(err.to_string()))?;
        let mutations = batch
            .column(mutation_idx)
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| io::Error::other("_skippr_mutation must be Utf8"))?;
        let orders = batch
            .column(order_idx)
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| io::Error::other("_skippr_order_token must be Utf8"))?;
        let mut upsert_mask = Vec::with_capacity(batch.num_rows());
        let mut delete_mask = Vec::with_capacity(batch.num_rows());
        for row in 0..batch.num_rows() {
            let key = business_key_for_row(&batch, &contract.business_key_columns, row)?;
            let order_token = validate_cdc_order_token(orders.value(row))?.to_string();
            let shard = cdc_state_shard(&key);
            let is_stale = state
                .loaded_shards
                .get(&shard)
                .and_then(|state| state.values.get(&key))
                .map(|seen| compare_cdc_order_tokens(seen, &order_token))
                .transpose()?
                .unwrap_or(false);
            if is_stale {
                upsert_mask.push(false);
                delete_mask.push(false);
                continue;
            }
            match mutations.value(row) {
                "delete" => {
                    upsert_mask.push(false);
                    delete_mask.push(true);
                }
                "snapshot" | "insert" | "update" => {
                    upsert_mask.push(true);
                    delete_mask.push(false);
                }
                other => {
                    return Err(io::Error::other(format!(
                        "unsupported CDC mutation '{}'",
                        other
                    )));
                }
            }
            state
                .loaded_shards
                .get_mut(&shard)
                .ok_or_else(|| {
                    io::Error::other(format!(
                        "Iceberg CDC state shard {} was not loaded before filtering",
                        shard
                    ))
                })?
                .values
                .insert(key.clone(), order_token.clone());
            if !state.delta.contains_key(&key) && state.delta.len() >= ICEBERG_MAX_ENVELOPE_KEYS {
                return Err(io::Error::other(format!(
                    "Iceberg grouped CDC envelope exceeds the {} unique-key session bound",
                    ICEBERG_MAX_ENVELOPE_KEYS
                )));
            }
            state.delta.insert(key, order_token);
        }
        let upsert = filter_record_batch(&batch, &BooleanArray::from(upsert_mask))
            .map_err(|err| io::Error::other(err.to_string()))?;
        if upsert.num_rows() > 0 {
            upsert_batches.push(upsert);
        }
        let delete = filter_record_batch(&batch, &BooleanArray::from(delete_mask))
            .map_err(|err| io::Error::other(err.to_string()))?;
        if delete.num_rows() > 0 {
            delete_batches.push(project_batch_columns(
                &delete,
                &contract.business_key_columns,
            )?);
        }
    }
    Ok((upsert_batches, delete_batches))
}

fn equality_ids_for_columns(
    table: &iceberg::table::Table,
    namespace: &str,
    columns: &[String],
) -> Result<Vec<i32>, io::Error> {
    if columns.is_empty() {
        return Err(io::Error::other(format!(
            "Iceberg CDC namespace '{}' requires business_key_columns",
            namespace
        )));
    }
    let schema = table.metadata().current_schema();
    columns
        .iter()
        .map(|column| {
            validate_flat_column_name(column)?;
            schema
                .field_by_name(column)
                .map(|field| field.id)
                .ok_or_else(|| {
                    io::Error::other(format!(
                        "Iceberg CDC business key '{}' is not present in namespace '{}'",
                        column, namespace
                    ))
                })
        })
        .collect()
}

fn grouped_snapshot_properties(manifest: &ObjectWriteManifest) -> HashMap<String, String> {
    HashMap::from([
        (
            SNAPSHOT_COMPACTION_ID.to_string(),
            manifest.compaction_id.clone(),
        ),
        (
            SNAPSHOT_IDEMPOTENCY_KEY.to_string(),
            manifest.idempotency_key.clone(),
        ),
        (
            SNAPSHOT_SCHEMA_FINGERPRINT.to_string(),
            manifest.schema_fingerprint.clone(),
        ),
        (
            SNAPSHOT_WAL_FINGERPRINT.to_string(),
            manifest
                .wal_refs_fingerprint_v2
                .clone()
                .unwrap_or_else(|| manifest.wal_refs_fingerprint.clone()),
        ),
        (
            SNAPSHOT_WAL_REF_COUNT.to_string(),
            manifest.wal_ref_count.to_string(),
        ),
    ])
}

fn table_has_grouped_snapshot(
    table: &iceberg::table::Table,
    manifest: &ObjectWriteManifest,
) -> bool {
    let expected = grouped_snapshot_properties(manifest);
    table.metadata().snapshots().any(|snapshot| {
        expected.iter().all(|(key, value)| {
            snapshot
                .summary()
                .additional_properties
                .get(key)
                .is_some_and(|actual| actual == value)
        })
    })
}

fn iceberg_table_suffix(namespace: &str) -> String {
    let raw = namespace.rsplit('.').next().unwrap_or(namespace);
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches('_').to_string();
    if trimmed.is_empty() {
        "table".to_string()
    } else {
        trimmed
    }
}

impl IcebergCatalogConfig {
    pub fn adapter_name(&self) -> &'static str {
        match self {
            IcebergCatalogConfig::Glue { .. } => "glue",
            IcebergCatalogConfig::Rest { .. } => "rest",
            IcebergCatalogConfig::Unity { .. } => "unity",
            IcebergCatalogConfig::Polaris { .. } => "polaris",
        }
    }
}

struct PreparedCdcStreams {
    upsert_stream: SendableRecordBatchStream,
    delete_stream: Option<SendableRecordBatchStream>,
    state_updates: HashMap<String, String>,
    upsert_rows: u64,
}

struct VecRecordBatchStream {
    schema: SchemaRef,
    batches: std::vec::IntoIter<RecordBatch>,
}

impl Stream for VecRecordBatchStream {
    type Item = Result<RecordBatch, DataFusionError>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> TaskPoll<Option<Self::Item>> {
        let this = self.get_mut();
        TaskPoll::Ready(this.batches.next().map(Ok))
    }
}

impl RecordBatchStream for VecRecordBatchStream {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
}

fn batch_stream(batches: Vec<RecordBatch>) -> SendableRecordBatchStream {
    let schema = batches
        .first()
        .map(|batch| batch.schema())
        .unwrap_or_else(|| Arc::new(arrow::datatypes::Schema::empty()));
    Box::pin(VecRecordBatchStream {
        schema,
        batches: batches.into_iter(),
    })
}

async fn collect_record_batches(
    mut stream: SendableRecordBatchStream,
) -> Result<Vec<RecordBatch>, io::Error> {
    let mut batches = Vec::new();
    while let Some(batch) = stream.next().await {
        batches.push(batch.map_err(|err| io::Error::other(err.to_string()))?);
    }
    Ok(batches)
}

fn plan_contract_equality_ids(
    table: &iceberg::table::Table,
    key_paths: &[FieldPath],
) -> Result<Vec<i32>, io::Error> {
    validate_flat_field_paths(key_paths)?;
    let schema = table.metadata().current_schema();
    let mut equality_ids = Vec::with_capacity(key_paths.len());
    for path in key_paths {
        let column = path.dotted();
        let field = schema.field_by_name(&column).ok_or_else(|| {
            io::Error::other(format!(
                "Iceberg equality key '{}' is not present in table schema",
                column
            ))
        })?;
        equality_ids.push(field.id);
    }
    Ok(equality_ids)
}

fn validate_flat_field_paths(key_paths: &[FieldPath]) -> Result<(), io::Error> {
    for path in key_paths {
        let column = path.dotted();
        validate_flat_column_name(&column)?;
    }
    Ok(())
}

fn validate_flat_column_name(column: &str) -> Result<(), io::Error> {
    if column.contains('.') {
        return Err(io::Error::other(format!(
            "Iceberg equality key '{}' is nested/dotted; nested equality keys need a real nested resolver before they are supported",
            column
        )));
    }
    Ok(())
}

fn validate_no_duplicate_batch_keys(
    batch: &RecordBatch,
    columns: &[String],
) -> Result<(), io::Error> {
    let mut seen = HashSet::new();
    for row in 0..batch.num_rows() {
        let key = business_key_for_row(batch, columns, row)?;
        if !seen.insert(key) {
            return Err(io::Error::other(format!(
                "Iceberg MergeByKey batch contains duplicate key at row {}; provide a deterministic source order or deduplicate upstream",
                row
            )));
        }
    }
    Ok(())
}

fn dedupe_batch_by_columns(
    batch: &RecordBatch,
    columns: &[String],
) -> Result<RecordBatch, io::Error> {
    let mut seen = HashSet::new();
    let mut keep = Vec::with_capacity(batch.num_rows());
    for row in 0..batch.num_rows() {
        let key = business_key_for_row(batch, columns, row)?;
        if seen.insert(key) {
            keep.push(row);
        }
    }
    let mut keep_mask = vec![false; batch.num_rows()];
    for row in keep {
        keep_mask[row] = true;
    }
    let mask = BooleanArray::from(keep_mask);
    filter_record_batch(batch, &mask).map_err(|err| io::Error::other(err.to_string()))
}

fn project_batch_columns(
    batch: &RecordBatch,
    columns: &[String],
) -> Result<RecordBatch, io::Error> {
    let schema = batch.schema();
    let mut fields = Vec::with_capacity(columns.len());
    let mut projected: Vec<ArrayRef> = Vec::with_capacity(columns.len());
    for column in columns {
        validate_flat_column_name(column)?;
        let idx = schema
            .index_of(column)
            .map_err(|err| io::Error::other(err.to_string()))?;
        fields.push(schema.field(idx).clone());
        projected.push(batch.column(idx).clone());
    }
    RecordBatch::try_new(Arc::new(arrow::datatypes::Schema::new(fields)), projected)
        .map_err(|err| io::Error::other(err.to_string()))
}

fn apply_iceberg_field_ids_to_batches(
    batches: Vec<RecordBatch>,
    iceberg_schema: &Schema,
) -> Result<Vec<RecordBatch>, io::Error> {
    batches
        .into_iter()
        .map(|batch| apply_iceberg_field_ids(batch, iceberg_schema))
        .collect()
}

fn iceberg_schema_has_all_fields(current: &Schema, desired: &Schema) -> bool {
    desired
        .as_struct()
        .fields()
        .iter()
        .all(|field| current.field_by_name(&field.name).is_some())
}

fn merge_iceberg_schema_missing_fields(
    namespace: &str,
    current: &Schema,
    desired: &Schema,
) -> Result<Schema, io::Error> {
    let mut fields: Vec<Arc<NestedField>> = current.as_struct().fields().iter().cloned().collect();
    let mut seen_field_ids = HashMap::<i32, ()>::new();
    for field in current.as_struct().fields() {
        collect_iceberg_field_ids(field, &mut seen_field_ids);
    }

    for field in desired.as_struct().fields() {
        if current.field_by_name(&field.name).is_none() {
            fields.push(remap_iceberg_field_ids(
                namespace,
                field,
                Vec::new(),
                &mut seen_field_ids,
            ));
        }
    }
    Schema::builder()
        .with_schema_id(0)
        .with_fields(fields)
        .build()
        .map_err(|err| io::Error::other(err.to_string()))
}

fn collect_iceberg_field_ids(field: &NestedField, seen_field_ids: &mut HashMap<i32, ()>) {
    seen_field_ids.insert(field.id, ());
    collect_iceberg_type_field_ids(&field.field_type, seen_field_ids);
}

fn collect_iceberg_type_field_ids(field_type: &Type, seen_field_ids: &mut HashMap<i32, ()>) {
    match field_type {
        Type::Primitive(_) => {}
        Type::Struct(struct_type) => {
            for child in struct_type.fields() {
                collect_iceberg_field_ids(child, seen_field_ids);
            }
        }
        Type::List(list_type) => {
            collect_iceberg_field_ids(&list_type.element_field, seen_field_ids);
        }
        Type::Map(map_type) => {
            collect_iceberg_field_ids(&map_type.key_field, seen_field_ids);
            collect_iceberg_field_ids(&map_type.value_field, seen_field_ids);
        }
    }
}

fn remap_iceberg_field_ids(
    namespace: &str,
    field: &NestedField,
    mut path: Vec<String>,
    seen_field_ids: &mut HashMap<i32, ()>,
) -> Arc<NestedField> {
    path.push(field.name.clone());
    let field_id = if field.id != 0 && !seen_field_ids.contains_key(&field.id) {
        field.id
    } else {
        unique_iceberg_field_id(namespace, &path, seen_field_ids)
    };
    seen_field_ids.insert(field_id, ());
    Arc::new(NestedField {
        id: field_id,
        name: field.name.clone(),
        required: field.required,
        field_type: Box::new(remap_iceberg_type_field_ids(
            namespace,
            &field.field_type,
            path,
            seen_field_ids,
        )),
        doc: field.doc.clone(),
        initial_default: field.initial_default.clone(),
        write_default: field.write_default.clone(),
    })
}

fn remap_iceberg_type_field_ids(
    namespace: &str,
    field_type: &Type,
    path: Vec<String>,
    seen_field_ids: &mut HashMap<i32, ()>,
) -> Type {
    match field_type {
        Type::Primitive(primitive) => Type::Primitive(primitive.clone()),
        Type::Struct(struct_type) => Type::Struct(iceberg::spec::StructType::new(
            struct_type
                .fields()
                .iter()
                .map(|child| {
                    remap_iceberg_field_ids(namespace, child, path.clone(), seen_field_ids)
                })
                .collect(),
        )),
        Type::List(list_type) => Type::List(ListType::new(remap_iceberg_field_ids(
            namespace,
            &list_type.element_field,
            path,
            seen_field_ids,
        ))),
        Type::Map(map_type) => Type::Map(MapType::new(
            remap_iceberg_field_ids(namespace, &map_type.key_field, path.clone(), seen_field_ids),
            remap_iceberg_field_ids(namespace, &map_type.value_field, path, seen_field_ids),
        )),
    }
}

fn apply_iceberg_field_ids(
    batch: RecordBatch,
    iceberg_schema: &Schema,
) -> Result<RecordBatch, io::Error> {
    let schema = batch.schema();
    let mut fields = Vec::with_capacity(schema.fields().len());
    for field in schema.fields().iter() {
        let mut annotated = field.as_ref().clone();
        validate_flat_column_name(field.name())?;
        let iceberg_field = iceberg_schema.field_by_name(field.name()).ok_or_else(|| {
            io::Error::other(format!(
                "Iceberg field '{}' is not present in table schema; schema must be evolved before writing",
                field.name()
            ))
        })?;
        let mut metadata = annotated.metadata().clone();
        metadata.insert(
            PARQUET_FIELD_ID_META_KEY.to_string(),
            iceberg_field.id.to_string(),
        );
        annotated = annotated.with_metadata(metadata);
        fields.push(Arc::new(annotated));
    }
    let annotated_schema = Arc::new(ArrowSchema::new_with_metadata(
        fields,
        schema.metadata().clone(),
    ));
    RecordBatch::try_new(annotated_schema, batch.columns().to_vec())
        .map_err(|err| io::Error::other(err.to_string()))
}

fn business_key_for_row(
    batch: &RecordBatch,
    business_key_columns: &[String],
    row: usize,
) -> Result<String, io::Error> {
    let mut values = Vec::with_capacity(business_key_columns.len());
    for column in business_key_columns {
        validate_flat_column_name(column)?;
        let idx = batch
            .schema()
            .index_of(column)
            .map_err(|err| io::Error::other(err.to_string()))?;
        let array = batch.column(idx);
        if array.is_null(row) {
            return Err(io::Error::other(format!(
                "CDC business key '{}' is null at row {}",
                column, row
            )));
        }
        let value = array_value_to_string(array.as_ref(), row)
            .map_err(|err| io::Error::other(err.to_string()))?;
        values.push(length_prefixed_key_part(column, &value));
    }
    Ok(values.concat())
}

fn length_prefixed_key_part(column: &str, value: &str) -> String {
    format!("{}:{}{}:{}", column.len(), column, value.len(), value)
}

fn validate_cdc_order_token(token: &str) -> Result<&str, io::Error> {
    if token.is_empty() || token.len() % 2 != 0 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(io::Error::other(format!(
            "CDC order token '{}' is not fixed-width hex",
            token
        )));
    }
    Ok(token)
}

fn compare_cdc_order_tokens(seen: &str, incoming: &str) -> Result<bool, io::Error> {
    validate_cdc_order_token(seen)?;
    validate_cdc_order_token(incoming)?;
    if seen.len() != incoming.len() {
        return Err(io::Error::other(format!(
            "CDC order token width changed from {} to {}; source order encoding must be fixed-width",
            seen.len(),
            incoming.len()
        )));
    }
    Ok(seen >= incoming)
}

fn is_s3_get_object_not_found_error(err: &SdkError<GetObjectError>) -> bool {
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

fn iceberg_schema_from_output_metadata(
    namespace: &str,
    metadata: &OutputMetadata,
) -> Result<Schema, io::Error> {
    let mut fields = Vec::new();
    let mut seen_field_ids = HashMap::<i32, ()>::new();
    for (_, field) in metadata.child_fields() {
        fields.push(Arc::new(field_to_nested_field(
            namespace,
            field,
            Vec::new(),
            &mut seen_field_ids,
        )?));
    }
    append_cdc_encoded_fields(namespace, &mut fields, &mut seen_field_ids);
    Schema::builder()
        .with_schema_id(0)
        .with_fields(fields)
        .build()
        .map_err(|err| io::Error::other(err.to_string()))
}

fn append_cdc_encoded_fields(
    namespace: &str,
    fields: &mut Vec<Arc<NestedField>>,
    seen_field_ids: &mut HashMap<i32, ()>,
) {
    for name in ["_skippr_mutation", "_skippr_order_token"] {
        if fields.iter().any(|field| field.name == name) {
            continue;
        }
        let field_id = unique_iceberg_field_id(namespace, &[name.to_string()], seen_field_ids);
        seen_field_ids.insert(field_id, ());
        fields.push(Arc::new(NestedField::new(
            field_id,
            name,
            Type::Primitive(PrimitiveType::String),
            true,
        )));
    }
}

fn unique_iceberg_field_id(
    namespace: &str,
    path: &[String],
    seen_field_ids: &HashMap<i32, ()>,
) -> i32 {
    let mut candidate = crate::lineage::deterministic_field_id(namespace, path);
    if candidate != 0 && !seen_field_ids.contains_key(&candidate) {
        return candidate;
    }

    let mut attempt = 1u32;
    loop {
        let mut salted_path = path.to_vec();
        salted_path.push(format!("__field_id_dedupe_{}", attempt));
        candidate = crate::lineage::deterministic_field_id(namespace, &salted_path);
        if candidate != 0 && !seen_field_ids.contains_key(&candidate) {
            return candidate;
        }
        attempt += 1;
    }
}

fn field_to_nested_field(
    namespace: &str,
    metadata: &OutputMetadata,
    mut path: Vec<String>,
    seen_field_ids: &mut HashMap<i32, ()>,
) -> Result<NestedField, io::Error> {
    path.push(metadata.out_field_name().to_string());
    let field_id = if metadata.field_id() != 0 && !seen_field_ids.contains_key(&metadata.field_id())
    {
        metadata.field_id()
    } else {
        unique_iceberg_field_id(namespace, &path, seen_field_ids)
    };
    seen_field_ids.insert(field_id, ());
    let field_type = match metadata.determined_type() {
        SkipprDataType::Record => {
            let mut children = Vec::new();
            for (_, child) in metadata.child_fields() {
                children.push(Arc::new(field_to_nested_field(
                    namespace,
                    child,
                    path.clone(),
                    seen_field_ids,
                )?));
            }
            Type::Struct(iceberg::spec::StructType::new(children))
        }
        SkipprDataType::Array => {
            let mut element_path = path.clone();
            element_path.push("element".to_string());
            let element_type = if metadata.determined_type_values() == Some(&SkipprDataType::Record)
            {
                metadata
                    .child_fields()
                    .find(|(name, _)| name.as_str() == "0")
                    .map(|(_, child)| {
                        field_to_nested_field(namespace, child, path.clone(), seen_field_ids)
                    })
                    .transpose()?
                    .map(|field| *field.field_type)
                    .unwrap_or_else(|| Type::Struct(iceberg::spec::StructType::new(Vec::new())))
            } else {
                primitive_type_for_skippr(
                    metadata
                        .determined_type_values()
                        .unwrap_or(&SkipprDataType::String),
                )
            };
            let element_field_id =
                unique_iceberg_field_id(namespace, &element_path, seen_field_ids);
            seen_field_ids.insert(element_field_id, ());
            Type::List(ListType::new(Arc::new(NestedField::list_element(
                element_field_id,
                element_type,
                true,
            ))))
        }
        SkipprDataType::Map => {
            let mut key_path = path.clone();
            key_path.push("key".to_string());
            let mut value_path = path.clone();
            value_path.push("value".to_string());
            let key_field_id = unique_iceberg_field_id(namespace, &key_path, seen_field_ids);
            seen_field_ids.insert(key_field_id, ());
            let value_field_id = unique_iceberg_field_id(namespace, &value_path, seen_field_ids);
            seen_field_ids.insert(value_field_id, ());
            Type::Map(MapType::new(
                Arc::new(NestedField::map_key_element(
                    key_field_id,
                    Type::Primitive(PrimitiveType::String),
                )),
                Arc::new(NestedField::map_value_element(
                    value_field_id,
                    primitive_type_for_skippr(
                        metadata
                            .determined_type_values()
                            .unwrap_or(&SkipprDataType::String),
                    ),
                    true,
                )),
            ))
        }
        typ => primitive_type_for_skippr(typ),
    };
    Ok(NestedField::new(
        field_id,
        metadata.out_field_name(),
        field_type,
        !metadata.nullable(),
    ))
}

fn iceberg_date_field_names(metadata: &OutputMetadata) -> HashSet<String> {
    metadata
        .child_fields()
        .filter(|(_, child)| *child.determined_type() == SkipprDataType::Date)
        .map(|(_, child)| child.out_field_name().to_string())
        .collect()
}

fn primitive_type_for_skippr(value: &SkipprDataType) -> Type {
    let primitive = match value {
        SkipprDataType::Boolean => PrimitiveType::Boolean,
        SkipprDataType::Byte
        | SkipprDataType::Short
        | SkipprDataType::Integer
        | SkipprDataType::Unknown
        | SkipprDataType::Null => PrimitiveType::Int,
        SkipprDataType::Long => PrimitiveType::Long,
        SkipprDataType::Float => PrimitiveType::Float,
        SkipprDataType::Double => PrimitiveType::Double,
        SkipprDataType::Decimal => PrimitiveType::Decimal {
            precision: 38,
            scale: 9,
        },
        SkipprDataType::Date => PrimitiveType::Date,
        SkipprDataType::Timestamp | SkipprDataType::TimestampMilli => PrimitiveType::Timestamp,
        SkipprDataType::Time => PrimitiveType::Time,
        SkipprDataType::Binary | SkipprDataType::Fixed => PrimitiveType::Binary,
        SkipprDataType::Uuid => PrimitiveType::Uuid,
        SkipprDataType::String
        | SkipprDataType::Json
        | SkipprDataType::Array
        | SkipprDataType::Map => PrimitiveType::String,
        SkipprDataType::Record => PrimitiveType::String,
    };
    Type::Primitive(primitive)
}

fn to_iceberg_s3_uri(uri: &str) -> String {
    if let Some(rest) = uri.strip_prefix("s3://") {
        format!("s3a://{rest}")
    } else {
        uri.to_string()
    }
}

fn parse_s3_uri(uri: &str) -> Result<(String, String), io::Error> {
    let without_scheme = uri
        .strip_prefix("s3://")
        .or_else(|| uri.strip_prefix("s3a://"))
        .ok_or_else(|| io::Error::other(format!("expected s3:// or s3a:// URI, got '{}'", uri)))?;
    let (bucket, key) = without_scheme
        .split_once('/')
        .ok_or_else(|| io::Error::other(format!("expected s3://bucket/key URI, got '{}'", uri)))?;
    Ok((bucket.to_string(), key.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use iceberg::memory::{MemoryCatalogBuilder, MEMORY_CATALOG_WAREHOUSE};
    use iceberg::spec::{DataFileBuilder, DataFileFormat, Struct};
    use serde_json::json;

    fn output_metadata(value: serde_json::Value) -> OutputMetadata {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn equal_or_older_schema_state_install_is_a_noop() {
        let metadata = output_metadata(json!({
            "out_field_name": "",
            "determined_type": "record",
            "determined_type_values": "",
            "fields": {}
        }));
        let initial = BTreeMap::from([("events".to_string(), metadata.clone())]);
        let replacement = BTreeMap::from([("users".to_string(), metadata)]);
        let mut state = InstalledIcebergSchemaState::default();

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
    fn merge_schema_remaps_appended_field_ids_against_nested_current_ids() {
        let current = Schema::builder()
            .with_schema_id(0)
            .with_fields(vec![Arc::new(NestedField::new(
                10,
                "detail",
                Type::Struct(iceberg::spec::StructType::new(vec![Arc::new(
                    NestedField::new(20, "inner", Type::Primitive(PrimitiveType::String), false),
                )])),
                false,
            ))])
            .build()
            .unwrap();
        let desired = Schema::builder()
            .with_schema_id(0)
            .with_fields(vec![Arc::new(NestedField::new(
                20,
                "new_top_level",
                Type::Primitive(PrimitiveType::Long),
                false,
            ))])
            .build()
            .unwrap();

        let merged = merge_iceberg_schema_missing_fields("cube_events", &current, &desired)
            .expect("schema merge should remap colliding field ids");
        let appended = merged.field_by_name("new_top_level").unwrap();

        assert_ne!(appended.id, 20);
        let mut seen = HashMap::new();
        for field in merged.as_struct().fields() {
            collect_iceberg_field_ids(field, &mut seen);
        }
        assert_eq!(seen.len(), 3);
    }

    #[test]
    fn iceberg_schema_preserves_required_field_id_and_scalar_type() {
        let metadata = output_metadata(json!({
            "out_field_name": "id",
            "determined_type": "long",
            "determined_type_values": "",
            "field_id": 10,
            "schema_id": 1,
            "lineage_id": "orders:id",
            "nullable": false,
            "default_value": null,
            "fields": {}
        }));

        let mut seen_field_ids = HashMap::new();
        let field =
            field_to_nested_field("orders", &metadata, Vec::new(), &mut seen_field_ids).unwrap();
        assert_eq!(field.id, 10);
        assert!(field.required);
        assert!(matches!(
            field.field_type.as_ref(),
            Type::Primitive(PrimitiveType::Long)
        ));
    }

    #[test]
    fn iceberg_schema_preserves_array_and_map_shapes() {
        let array_metadata = output_metadata(json!({
            "out_field_name": "tags",
            "determined_type": "array",
            "determined_type_values": "string",
            "field_id": 11,
            "schema_id": 1,
            "lineage_id": "orders:tags",
            "nullable": true,
            "default_value": null,
            "fields": {}
        }));
        let map_metadata = output_metadata(json!({
            "out_field_name": "attrs",
            "determined_type": "map",
            "determined_type_values": "string",
            "field_id": 12,
            "schema_id": 1,
            "lineage_id": "orders:attrs",
            "nullable": true,
            "default_value": null,
            "fields": {}
        }));

        let mut seen_field_ids = HashMap::new();
        let array_field =
            field_to_nested_field("orders", &array_metadata, Vec::new(), &mut seen_field_ids)
                .unwrap();
        let map_field =
            field_to_nested_field("orders", &map_metadata, Vec::new(), &mut seen_field_ids)
                .unwrap();

        assert!(matches!(array_field.field_type.as_ref(), Type::List(_)));
        assert!(matches!(map_field.field_type.as_ref(), Type::Map(_)));
    }

    #[test]
    fn iceberg_table_suffix_uses_last_namespace_segment_and_sanitizes() {
        assert_eq!(
            iceberg_table_suffix("postgres.type_matrix_orders"),
            "type_matrix_orders"
        );
        assert_eq!(iceberg_table_suffix("S3.Raw-Orders"), "raw_orders");
    }

    #[test]
    fn is_duplicate_iceberg_file_error_detects_wal_replay_commit_clash() {
        assert!(is_duplicate_iceberg_file_error(
            "DataInvalid => Cannot add files that are already referenced by table, files: s3a://bucket/x.parquet"
        ));
        assert!(!is_duplicate_iceberg_file_error("AccessDenied"));
    }

    #[test]
    fn commit_files_partition_routes_data_only_to_fast_append_path() {
        let table = make_v2_minimal_table_for_tests();
        let data_file = DataFileBuilder::default()
            .content(DataContentType::Data)
            .file_path("s3://bucket/data.parquet".to_string())
            .file_format(DataFileFormat::Parquet)
            .file_size_in_bytes(100)
            .record_count(1)
            .partition_spec_id(table.metadata().default_partition_spec_id())
            .partition(Struct::empty())
            .build()
            .unwrap();
        let (data_files, delete_files) = partition_commit_files(vec![data_file]);
        assert_eq!(data_files.len(), 1);
        assert!(delete_files.is_empty());
    }

    #[test]
    fn commit_files_partition_routes_mixed_files_to_equality_delta_path() {
        let table = make_v2_minimal_table_for_tests();
        let data_file = DataFileBuilder::default()
            .content(DataContentType::Data)
            .file_path("s3://bucket/data.parquet".to_string())
            .file_format(DataFileFormat::Parquet)
            .file_size_in_bytes(100)
            .record_count(1)
            .partition_spec_id(table.metadata().default_partition_spec_id())
            .partition(Struct::empty())
            .build()
            .unwrap();
        let delete_file = DataFileBuilder::default()
            .content(DataContentType::EqualityDeletes)
            .file_path("s3://bucket/delete.parquet".to_string())
            .file_format(DataFileFormat::Parquet)
            .file_size_in_bytes(50)
            .record_count(1)
            .partition_spec_id(table.metadata().default_partition_spec_id())
            .partition(Struct::empty())
            .equality_ids(Some(vec![1]))
            .build()
            .unwrap();
        let (data_files, delete_files) =
            partition_commit_files(vec![data_file.clone(), delete_file.clone()]);
        assert_eq!(data_files, vec![data_file]);
        assert_eq!(delete_files, vec![delete_file]);
    }

    #[test]
    fn project_batch_columns_keeps_only_equality_keys() {
        let batch = RecordBatch::try_from_iter(vec![
            (
                "id",
                Arc::new(arrow::array::Int32Array::from(vec![2])) as ArrayRef,
            ),
            (
                "_skippr_mutation",
                Arc::new(StringArray::from(vec!["delete"])) as ArrayRef,
            ),
            (
                "_skippr_order_token",
                Arc::new(StringArray::from(vec!["00000001"])) as ArrayRef,
            ),
        ])
        .unwrap();

        let projected = project_batch_columns(&batch, &[String::from("id")]).unwrap();

        assert_eq!(projected.num_columns(), 1);
        assert_eq!(projected.schema().field(0).name(), "id");
        assert_eq!(projected.num_rows(), 1);
    }

    #[test]
    fn apply_iceberg_field_ids_annotates_delete_key_schema() {
        let iceberg_schema = Schema::builder()
            .with_schema_id(1)
            .with_fields(vec![
                NestedField::required(7, "id", Type::Primitive(PrimitiveType::Int)).into(),
                NestedField::optional(8, "name", Type::Primitive(PrimitiveType::String)).into(),
            ])
            .build()
            .unwrap();
        let batch = RecordBatch::try_from_iter(vec![
            (
                "id",
                Arc::new(arrow::array::Int32Array::from(vec![2])) as ArrayRef,
            ),
            (
                "_skippr_mutation",
                Arc::new(StringArray::from(vec!["delete"])) as ArrayRef,
            ),
        ])
        .unwrap();
        let projected = project_batch_columns(&batch, &[String::from("id")]).unwrap();

        let annotated = apply_iceberg_field_ids(projected, &iceberg_schema).unwrap();

        assert_eq!(
            annotated
                .schema()
                .field(0)
                .metadata()
                .get(PARQUET_FIELD_ID_META_KEY),
            Some(&"7".to_string())
        );
    }

    #[test]
    fn project_batch_columns_rejects_nested_equality_key() {
        let batch = RecordBatch::try_from_iter(vec![(
            "id",
            Arc::new(arrow::array::Int32Array::from(vec![2])) as ArrayRef,
        )])
        .unwrap();

        let err = project_batch_columns(&batch, &[String::from("customer.id")]).unwrap_err();
        assert!(err.to_string().contains("nested/dotted"));
    }

    #[test]
    fn merge_by_key_duplicate_batch_keys_fail_before_write() {
        let batch = RecordBatch::try_from_iter(vec![(
            "id",
            Arc::new(arrow::array::Int32Array::from(vec![2, 2])) as ArrayRef,
        )])
        .unwrap();

        let err = validate_no_duplicate_batch_keys(&batch, &[String::from("id")]).unwrap_err();
        assert!(err.to_string().contains("duplicate key"));
    }

    #[test]
    fn structured_business_key_encoding_avoids_delimiter_collision() {
        assert_ne!(
            length_prefixed_key_part("a", "1|b=2"),
            length_prefixed_key_part("a=1|b", "2")
        );
    }

    #[test]
    fn cdc_order_token_comparison_requires_fixed_width_hex() {
        assert!(compare_cdc_order_tokens("0002", "0001").unwrap());
        assert!(!compare_cdc_order_tokens("0001", "0002").unwrap());
        assert!(compare_cdc_order_tokens("02", "0001").is_err());
        assert!(validate_cdc_order_token("not_hex").is_err());
    }

    #[test]
    fn cdc_state_load_only_treats_not_found_as_empty() {
        assert!(is_s3_not_found_code(Some("NoSuchKey")));
        assert!(is_s3_not_found_code(Some("NotFound")));
        assert!(is_s3_not_found_error_text("service error NoSuchKey"));
        assert!(is_s3_not_found_error_text("status code: 404"));
        assert!(!is_s3_not_found_code(Some("AccessDenied")));
        assert!(!is_s3_not_found_error_text("AccessDenied"));
    }

    fn grouped_manifest(id: &str) -> ObjectWriteManifest {
        ObjectWriteManifest {
            compaction_id: format!("compaction-{id}"),
            idempotency_key: format!("apply-{id}"),
            schema_fingerprint: "schema-v7".to_string(),
            wal_refs_fingerprint: "legacy-wal-fingerprint".to_string(),
            wal_ref_count: 2,
            identity_version: Some(2),
            wal_refs_fingerprint_v2: Some("canonical-wal-fingerprint".to_string()),
            has_cdc_metadata: true,
        }
    }

    fn exact_cdc_contract() -> skippr_runtime_sdk::plugins::cdc::NamespaceContract {
        skippr_runtime_sdk::plugins::cdc::NamespaceContract {
            namespace: "orders".to_string(),
            business_key_columns: vec!["id".to_string()],
            effective_guarantee: EffectiveGuarantee::ExactOnceFinalState,
            order_token_semantics: Default::default(),
            null_key_policy: Default::default(),
            requires_skippr_system_columns: true,
        }
    }

    fn cdc_batch(ids: Vec<i32>, mutations: Vec<&str>, tokens: Vec<&str>) -> RecordBatch {
        RecordBatch::try_from_iter(vec![
            (
                "id",
                Arc::new(arrow::array::Int32Array::from(ids)) as ArrayRef,
            ),
            (
                "value",
                Arc::new(StringArray::from_iter_values(
                    ["a", "b", "c"].into_iter().take(mutations.len()),
                )) as ArrayRef,
            ),
            (
                "_skippr_mutation",
                Arc::new(StringArray::from(mutations)) as ArrayRef,
            ),
            (
                "_skippr_order_token",
                Arc::new(StringArray::from(tokens)) as ArrayRef,
            ),
        ])
        .unwrap()
    }

    fn empty_loaded_cdc_state(keys: &[&str]) -> CdcStateSession {
        let mut state = CdcStateSession::default();
        for key in keys {
            state
                .loaded_shards
                .entry(cdc_state_shard(key))
                .or_insert_with(|| LoadedCdcStateShard {
                    values: BTreeMap::new(),
                    etag: None,
                });
        }
        state
    }

    #[test]
    fn grouped_cdc_filters_stale_rows_and_splits_upserts_from_deletes() {
        let contract = exact_cdc_contract();
        let keys = ["2:id1:1", "2:id1:2", "2:id1:3"];
        let mut state = empty_loaded_cdc_state(&keys);
        state
            .loaded_shards
            .get_mut(&cdc_state_shard(keys[2]))
            .unwrap()
            .values
            .insert(keys[2].to_string(), "00000005".to_string());

        let (upserts, deletes) = filter_grouped_cdc_batches(
            &contract,
            vec![cdc_batch(
                vec![1, 2, 3],
                vec!["update", "delete", "update"],
                vec!["00000002", "00000003", "00000004"],
            )],
            &mut state,
        )
        .unwrap();

        assert_eq!(upserts.iter().map(RecordBatch::num_rows).sum::<usize>(), 1);
        assert_eq!(deletes.iter().map(RecordBatch::num_rows).sum::<usize>(), 1);
        assert_eq!(deletes[0].num_columns(), 1);
        assert_eq!(state.delta.len(), 2);
        assert!(!state.delta.contains_key(keys[2]));
    }

    #[test]
    fn grouped_cdc_applies_order_filter_across_transport_chunks() {
        let contract = exact_cdc_contract();
        let key = "2:id1:1";
        let mut state = empty_loaded_cdc_state(&[key]);
        let (first_upserts, _) = filter_grouped_cdc_batches(
            &contract,
            vec![cdc_batch(vec![1], vec!["update"], vec!["00000002"])],
            &mut state,
        )
        .unwrap();
        let (late_upserts, late_deletes) = filter_grouped_cdc_batches(
            &contract,
            vec![cdc_batch(vec![1], vec!["delete"], vec!["00000001"])],
            &mut state,
        )
        .unwrap();

        assert_eq!(first_upserts[0].num_rows(), 1);
        assert!(late_upserts.is_empty());
        assert!(late_deletes.is_empty());
        assert_eq!(state.delta.get(key).map(String::as_str), Some("00000002"));
    }

    #[test]
    fn grouped_file_rolling_is_deterministic_and_row_bounded() {
        let rows = ICEBERG_FILE_MAX_ROWS * 2 + 1;
        let batch = RecordBatch::try_from_iter(vec![(
            "id",
            Arc::new(arrow::array::Int32Array::from_iter_values(
                0..i32::try_from(rows).unwrap(),
            )) as ArrayRef,
        )])
        .unwrap();

        let rolls = roll_record_batches(vec![batch]);

        assert_eq!(rolls.len(), 3);
        assert_eq!(
            rolls
                .iter()
                .flat_map(|roll| roll.iter())
                .map(RecordBatch::num_rows)
                .sum::<usize>(),
            rows
        );
        assert!(rolls.iter().all(|roll| {
            roll.iter().map(RecordBatch::num_rows).sum::<usize>() <= ICEBERG_FILE_MAX_ROWS
        }));
    }

    #[test]
    fn grouped_file_names_are_stable_and_content_scoped() {
        assert_eq!(
            grouped_file_identity("apply-1", PendingFileContent::Data, 3),
            "apply-1-data-00000003"
        );
        assert_eq!(
            grouped_file_identity("apply-1", PendingFileContent::EqualityDeletes, 3),
            "apply-1-delete-00000003"
        );
        assert_eq!(
            grouped_pending_key("metadata/apply-1.skippr-manifest.json"),
            "metadata/apply-1.skippr-manifest.json.pending-v2.json"
        );
    }

    #[test]
    fn legacy_grouped_file_names_remain_replay_discoverable() {
        assert_eq!(
            legacy_grouped_object_keys("/warehouse/orders/", "apply-1"),
            vec![
                "warehouse/orders/data/apply-1.parquet",
                "warehouse/orders/delete/apply-1.parquet",
                "warehouse/orders/data/apply-1-chunk-00000000.parquet",
                "warehouse/orders/delete/apply-1-chunk-00000000.parquet",
            ]
        );
    }

    #[test]
    fn grouped_pending_round_trip_preserves_crash_recovery_delta() {
        let pending = IcebergGroupedPending {
            version: ICEBERG_GROUPED_PENDING_VERSION,
            manifest: grouped_manifest("7"),
            namespace: "orders".to_string(),
            schema_version: 7,
            transport_chunk_count: 3,
            rows: 2,
            files: vec![
                PendingIcebergFile {
                    content: PendingFileContent::Data,
                    file_uri: "s3a://bucket/orders/data/apply-7-data-00000000.parquet".to_string(),
                    rows: 2,
                    bytes: 100,
                    equality_ids: Vec::new(),
                },
                PendingIcebergFile {
                    content: PendingFileContent::EqualityDeletes,
                    file_uri: "s3a://bucket/orders/delete/apply-7-delete-00000000.parquet"
                        .to_string(),
                    rows: 1,
                    bytes: 50,
                    equality_ids: vec![1],
                },
            ],
            state_delta: BTreeMap::from([
                ("2:id1:1".to_string(), "00000002".to_string()),
                ("2:id1:2".to_string(), "00000003".to_string()),
            ]),
        };

        let bytes = serde_json::to_vec(&pending).unwrap();
        let restored: IcebergGroupedPending = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(restored, pending);
        assert!(restored.manifest.matches_manifest(&grouped_manifest("7")));
    }

    #[test]
    fn grouped_merge_detects_duplicate_keys_across_transport_chunks() {
        let first = RecordBatch::try_from_iter(vec![(
            "id",
            Arc::new(arrow::array::Int32Array::from(vec![1])) as ArrayRef,
        )])
        .unwrap();
        let second = first.clone();
        let mut seen = HashSet::new();
        prepare_grouped_policy_batches(
            vec![first],
            WritePolicy::MergeByKey,
            &[String::from("id")],
            &mut seen,
        )
        .unwrap();

        let error = prepare_grouped_policy_batches(
            vec![second],
            WritePolicy::MergeByKey,
            &[String::from("id")],
            &mut seen,
        )
        .unwrap_err();

        assert!(error.to_string().contains("duplicate key"));
    }

    #[test]
    fn cdc_state_sharding_is_stable_and_bounded() {
        let shard = cdc_state_shard("2:id1:7");
        assert_eq!(shard, cdc_state_shard("2:id1:7"));
        assert!(shard < ICEBERG_CDC_STATE_SHARDS);
        assert_eq!(
            ObjectWriterConfig::default().transport_memory_bound_bytes()
                * ICEBERG_MAX_FILE_WRITES_IN_FLIGHT,
            160 * 1024 * 1024
        );
    }

    #[test]
    fn grouped_snapshot_identity_covers_parent_envelope() {
        let first = grouped_snapshot_properties(&grouped_manifest("1"));
        let replay = grouped_snapshot_properties(&grouped_manifest("1"));
        let other = grouped_snapshot_properties(&grouped_manifest("2"));

        assert_eq!(first, replay);
        assert_ne!(first, other);
        assert_eq!(
            first.get(SNAPSHOT_WAL_FINGERPRINT).map(String::as_str),
            Some("canonical-wal-fingerprint")
        );
    }

    #[test]
    fn installed_schema_version_invalidates_table_cache_only_when_advanced() {
        assert!(!schema_version_invalidates_table_cache(7, 6));
        assert!(!schema_version_invalidates_table_cache(7, 7));
        assert!(schema_version_invalidates_table_cache(7, 8));
    }

    #[tokio::test]
    async fn multiple_files_build_one_equality_delta_snapshot() {
        let catalog = MemoryCatalogBuilder::default()
            .load(
                "test",
                HashMap::from([(
                    MEMORY_CATALOG_WAREHOUSE.to_string(),
                    "memory://warehouse".to_string(),
                )]),
            )
            .await
            .unwrap();
        let namespace = NamespaceIdent::from_strs(["test"]).unwrap();
        catalog
            .create_namespace(&namespace, HashMap::new())
            .await
            .unwrap();
        let table = catalog
            .create_table(
                &namespace,
                TableCreation::builder()
                    .name("orders".to_string())
                    .schema(
                        Schema::builder()
                            .with_schema_id(1)
                            .with_fields(vec![NestedField::required(
                                1,
                                "id",
                                Type::Primitive(PrimitiveType::Int),
                            )
                            .into()])
                            .build()
                            .unwrap(),
                    )
                    .build(),
            )
            .await
            .unwrap();
        let data_file = |path: &str| {
            DataFileBuilder::default()
                .content(DataContentType::Data)
                .file_path(path.to_string())
                .file_format(DataFileFormat::Parquet)
                .file_size_in_bytes(100)
                .record_count(1)
                .partition_spec_id(table.metadata().default_partition_spec_id())
                .partition(Struct::empty())
                .build()
                .unwrap()
        };
        let delete_file = DataFileBuilder::default()
            .content(DataContentType::EqualityDeletes)
            .file_path("s3://bucket/delete.parquet".to_string())
            .file_format(DataFileFormat::Parquet)
            .file_size_in_bytes(50)
            .record_count(1)
            .partition_spec_id(table.metadata().default_partition_spec_id())
            .partition(Struct::empty())
            .equality_ids(Some(vec![1]))
            .build()
            .unwrap();
        let action = Transaction::new(&table)
            .equality_delta_append()
            .set_snapshot_properties(grouped_snapshot_properties(&grouped_manifest("1")))
            .add_data_files([
                data_file("s3://bucket/data-0.parquet"),
                data_file("s3://bucket/data-1.parquet"),
            ])
            .add_delete_files([delete_file]);
        let tx = action.apply(Transaction::new(&table)).unwrap();

        let committed = tx.commit(&catalog).await.unwrap();

        assert_eq!(committed.metadata().snapshots().len(), 1);
        let summary = committed.metadata().current_snapshot().unwrap().summary();
        assert_eq!(
            summary
                .additional_properties
                .get("added-data-files")
                .map(String::as_str),
            Some("2")
        );
        assert_eq!(
            summary
                .additional_properties
                .get("added-delete-files")
                .map(String::as_str),
            Some("1")
        );
        assert!(table_has_grouped_snapshot(
            &committed,
            &grouped_manifest("1")
        ));
    }
}

fn is_duplicate_iceberg_file_error(err: &str) -> bool {
    err.contains("Cannot add files that are already referenced by table")
}

fn partition_commit_files(
    commit_files: Vec<iceberg::spec::DataFile>,
) -> (Vec<iceberg::spec::DataFile>, Vec<iceberg::spec::DataFile>) {
    commit_files
        .into_iter()
        .partition(|file| file.content_type() == DataContentType::Data)
}

#[cfg(test)]
fn make_v2_minimal_table_for_tests() -> iceberg::table::Table {
    use std::fs::File;
    use std::io::BufReader;

    use std::sync::Arc;

    use iceberg::io::{FileIOBuilder, MemoryStorageFactory};
    use iceberg::spec::TableMetadata;
    use iceberg::TableIdent;

    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../third_party/iceberg/testdata/table_metadata");
    let file = File::open(manifest_dir.join("TableMetadataV2ValidMinimal.json")).unwrap();
    let reader = BufReader::new(file);
    let metadata = serde_json::from_reader::<_, TableMetadata>(reader).unwrap();
    iceberg::table::Table::builder()
        .metadata(metadata)
        .metadata_location("s3://bucket/test/location/metadata/v1.json".to_string())
        .identifier(TableIdent::from_strs(["ns1", "test1"]).unwrap())
        .file_io(FileIOBuilder::new(Arc::new(MemoryStorageFactory)).build())
        .build()
        .unwrap()
}
