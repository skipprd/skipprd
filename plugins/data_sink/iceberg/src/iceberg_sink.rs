use std::collections::{BTreeMap, HashMap};
use std::io;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll as TaskPoll};
use std::time::{Duration, SystemTime};

use arrow::array::{Array, BooleanArray, RecordBatch, StringArray};
use arrow::compute::filter_record_batch;
use arrow::datatypes::SchemaRef;
use arrow::util::display::array_value_to_string;
use async_trait::async_trait;
use aws_sdk_glue::types::TableInput;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client as S3Client;
use datafusion::error::DataFusionError;
use datafusion::execution::SendableRecordBatchStream;
use datafusion::physical_plan::RecordBatchStream;
use futures::Stream;
use futures::StreamExt;
use iceberg::spec::{
    DataContentType, DataFileBuilder, DataFileFormat, FormatVersion, ListType, ManifestContentType,
    ManifestListWriter, ManifestWriterBuilder, MapType, NestedField, Operation, PrimitiveType,
    Schema, Snapshot, SnapshotReference, SnapshotRetention, Struct, Summary, Type, MAIN_BRANCH,
};
use iceberg::transaction::{ApplyTransactionAction, Transaction};
use iceberg::{
    Catalog, CatalogBuilder, MetadataLocation, NamespaceIdent, TableCreation, TableIdent,
    TableUpdate,
};
use iceberg_catalog_glue::{GlueCatalog, GlueCatalogBuilder, GLUE_CATALOG_PROP_CATALOG_ID};
use iceberg_catalog_glue::{AWS_REGION_NAME, GLUE_CATALOG_PROP_WAREHOUSE};
use serde_derive::Deserialize;
use tokio::sync::RwLock;
use tracing::{info, warn};
use uuid::Uuid;

use crate::buffer::BufferChunker;
use crate::discover::{OutputMetadata, SkipprDataType};
use crate::helpers::configuration::DataSinkPluginConfig;
use crate::plugins::cdc::EffectiveGuarantee;
use crate::plugins::{DataSink, SchemaSink};
use skippr_core::runtime_plugins::protocol::{RuntimeBinding, RuntimeExecutionContext};

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
    version: u64,
    namespaces: BTreeMap<String, OutputMetadata>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IcebergCommitMode {
    FastAppend,
    RowDelta,
}

pub struct DataSinkIcebergPlugin {
    context: RuntimeExecutionContext,
    binding: RuntimeBinding,
    #[allow(dead_code)]
    buffer_name: String,
    config: DataSinkIcebergPluginConfig,
    s3_client: S3Client,
    schema_state: RwLock<InstalledIcebergSchemaState>,
}

#[async_trait]
impl DataSink for DataSinkIcebergPlugin {
    async fn sync(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
        cdc_ctx: Option<&crate::plugins::cdc::SyncContext>,
    ) -> Result<(), io::Error> {
        let stream = match cdc_ctx {
            Some(ctx) => super::cdc_encode::augment_stream_with_cdc_columns(stream, &ctx.part_meta),
            None => stream,
        };
        self.native_append(stream, filename, cdc_ctx).await
    }

    fn capability(&self) -> Option<&'static crate::plugins::cdc::SinkCapability> {
        Some(&crate::plugins::cdc::sink_capabilities::ICEBERG)
    }

    async fn install_schema_state(
        &self,
        schema_version: u64,
        namespaces: &BTreeMap<String, OutputMetadata>,
    ) -> Result<(), io::Error> {
        let mut guard = self.schema_state.write().await;
        guard.version = schema_version;
        guard.namespaces = namespaces.clone();
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
        self.ensure_table(&catalog, namespace, metadata).await?;
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
        })
    }

    async fn native_append(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
        cdc_ctx: Option<&crate::plugins::cdc::SyncContext>,
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

        let mut commit_files = Vec::new();
        let mut row_count = 0;
        if upsert_rows != Some(0) {
            let parquet_bytes = crate::parquet_util::serialize_to_parquet(data_stream).await?;
            row_count = parquet_bytes.meta_data.num_rows as u64;
            if row_count > 0 {
                let data_file_uri = self
                    .write_parquet_file(&namespace, "data", &filename, parquet_bytes.bytes.clone())
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
            if let Some(equality_ids) = self.plan_cdc_commit(&table, &namespace, cdc_ctx).await? {
                let delete_bytes = crate::parquet_util::serialize_to_parquet(delete_stream).await?;
                let delete_row_count = delete_bytes.meta_data.num_rows as u64;
                if delete_row_count > 0 {
                    let delete_file_uri = self
                        .write_parquet_file(&namespace, "delete", &filename, delete_bytes.bytes)
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
        }

        if commit_files.is_empty() {
            info!(
                "Iceberg commit skipped empty or stale batch for namespace '{}'",
                namespace
            );
            return Ok(());
        }

        let committed = self
            .commit_files_with_retries(&catalog, table.identifier().clone(), commit_files)
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

    async fn commit_files_with_retries(
        &self,
        catalog: &GlueCatalog,
        table_ident: TableIdent,
        files: Vec<iceberg::spec::DataFile>,
    ) -> Result<iceberg::table::Table, io::Error> {
        match commit_mode_for_files(&files) {
            IcebergCommitMode::FastAppend => {
                self.commit_append_with_retries(catalog, table_ident, files)
                    .await
            }
            IcebergCommitMode::RowDelta => {
                self.commit_row_delta_with_retries(catalog, table_ident, files)
                    .await
            }
        }
    }

    async fn commit_append_with_retries(
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

    async fn commit_row_delta_with_retries(
        &self,
        catalog: &GlueCatalog,
        table_ident: TableIdent,
        files: Vec<iceberg::spec::DataFile>,
    ) -> Result<iceberg::table::Table, io::Error> {
        let mut last_err: Option<String> = None;
        for attempt in 1..=3 {
            let table = catalog
                .load_table(&table_ident)
                .await
                .map_err(|err| io::Error::other(err.to_string()))?;
            match self.commit_row_delta_once(catalog, &table, &files).await {
                Ok(table) => return Ok(table),
                Err(err) => {
                    let err = err.to_string();
                    warn!(
                        "Iceberg row-delta commit attempt {} failed for {}: {}",
                        attempt, table_ident, err
                    );
                    last_err = Some(err);
                }
            }
        }
        Err(io::Error::other(format!(
            "Iceberg row-delta commit failed after retries for {}: {}",
            table_ident,
            last_err.unwrap_or_else(|| "unknown error".to_string())
        )))
    }

    async fn commit_row_delta_once(
        &self,
        catalog: &GlueCatalog,
        table: &iceberg::table::Table,
        files: &[iceberg::spec::DataFile],
    ) -> Result<iceberg::table::Table, io::Error> {
        if table.metadata().format_version() != FormatVersion::V2 {
            return Err(io::Error::other(
                "Iceberg CDC equality deletes require table format v2",
            ));
        }

        let snapshot_id = generate_snapshot_id(table);
        let commit_uuid = Uuid::new_v4();
        let next_sequence_number = table.metadata().next_sequence_number();
        let data_files: Vec<_> = files
            .iter()
            .filter(|file| file.content_type() == DataContentType::Data)
            .cloned()
            .collect();
        let delete_files: Vec<_> = files
            .iter()
            .filter(|file| file.content_type() != DataContentType::Data)
            .cloned()
            .collect();

        let mut manifests = Vec::new();
        if !data_files.is_empty() {
            manifests.push(
                self.write_added_manifest(
                    table,
                    snapshot_id,
                    commit_uuid,
                    0,
                    next_sequence_number,
                    ManifestContentType::Data,
                    data_files,
                )
                .await?,
            );
        }
        if !delete_files.is_empty() {
            manifests.push(
                self.write_added_manifest(
                    table,
                    snapshot_id,
                    commit_uuid,
                    1,
                    next_sequence_number,
                    ManifestContentType::Deletes,
                    delete_files,
                )
                .await?,
            );
        }

        let manifest_list_path = format!(
            "{}/metadata/snap-{}-0-{}.{}",
            table.metadata().location(),
            snapshot_id,
            commit_uuid,
            DataFileFormat::Avro
        );
        let mut manifest_list_writer = ManifestListWriter::v2(
            table
                .file_io()
                .new_output(manifest_list_path.clone())
                .map_err(|err| io::Error::other(err.to_string()))?,
            snapshot_id,
            table.metadata().current_snapshot_id(),
            next_sequence_number,
        );
        manifest_list_writer
            .add_manifests(manifests.into_iter())
            .map_err(|err| io::Error::other(err.to_string()))?;
        manifest_list_writer
            .close()
            .await
            .map_err(|err| io::Error::other(err.to_string()))?;

        let snapshot = Snapshot::builder()
            .with_manifest_list(manifest_list_path)
            .with_snapshot_id(snapshot_id)
            .with_parent_snapshot_id(table.metadata().current_snapshot_id())
            .with_sequence_number(next_sequence_number)
            .with_summary(snapshot_summary_for_files(files))
            .with_schema_id(table.metadata().current_schema_id())
            .with_timestamp_ms(current_time_millis())
            .build();
        let updates = vec![
            TableUpdate::AddSnapshot { snapshot },
            TableUpdate::SetSnapshotRef {
                ref_name: MAIN_BRANCH.to_string(),
                reference: SnapshotReference::new(
                    snapshot_id,
                    SnapshotRetention::branch(None, None, None),
                ),
            },
        ];

        let current_metadata_location = table
            .metadata_location_result()
            .map_err(|err| io::Error::other(err.to_string()))?
            .to_string();
        let staged_metadata_location = MetadataLocation::from_str(&current_metadata_location)
            .map_err(|err| io::Error::other(err.to_string()))?
            .with_next_version()
            .to_string();
        let mut metadata_builder = table
            .metadata()
            .clone()
            .into_builder(Some(current_metadata_location.clone()));
        for update in updates {
            metadata_builder = update
                .apply(metadata_builder)
                .map_err(|err| io::Error::other(err.to_string()))?;
        }
        let staged_metadata = metadata_builder
            .build()
            .map_err(|err| io::Error::other(err.to_string()))?
            .metadata;
        staged_metadata
            .write_to(table.file_io(), &staged_metadata_location)
            .await
            .map_err(|err| io::Error::other(err.to_string()))?;

        self.update_glue_metadata_location(
            table.identifier(),
            &current_metadata_location,
            &staged_metadata_location,
        )
        .await?;
        catalog
            .load_table(table.identifier())
            .await
            .map_err(|err| io::Error::other(err.to_string()))
    }

    async fn write_added_manifest(
        &self,
        table: &iceberg::table::Table,
        snapshot_id: i64,
        commit_uuid: Uuid,
        manifest_idx: u64,
        sequence_number: i64,
        content: ManifestContentType,
        files: Vec<iceberg::spec::DataFile>,
    ) -> Result<iceberg::spec::ManifestFile, io::Error> {
        let manifest_path = format!(
            "{}/metadata/{}-m{}.{}",
            table.metadata().location(),
            commit_uuid,
            manifest_idx,
            DataFileFormat::Avro
        );
        let output = table
            .file_io()
            .new_output(manifest_path)
            .map_err(|err| io::Error::other(err.to_string()))?;
        let builder = ManifestWriterBuilder::new(
            output,
            Some(snapshot_id),
            None,
            table.metadata().current_schema().clone(),
            table.metadata().default_partition_spec().as_ref().clone(),
        );
        let mut writer = match content {
            ManifestContentType::Data => builder.build_v2_data(),
            ManifestContentType::Deletes => builder.build_v2_deletes(),
        };
        for file in files {
            writer
                .add_file(file, sequence_number)
                .map_err(|err| io::Error::other(err.to_string()))?;
        }
        writer
            .write_manifest_file()
            .await
            .map_err(|err| io::Error::other(err.to_string()))
    }

    async fn update_glue_metadata_location(
        &self,
        table_ident: &TableIdent,
        current_metadata_location: &str,
        staged_metadata_location: &str,
    ) -> Result<(), io::Error> {
        let catalog_namespace = self.catalog_namespace()?;
        let glue_client = self.glue_client().await;
        let mut get_table = glue_client
            .get_table()
            .database_name(&catalog_namespace)
            .name(table_ident.name());
        if let Some(catalog_id) = self.glue_catalog_id() {
            get_table = get_table.catalog_id(catalog_id);
        }
        let glue_table = get_table
            .send()
            .await
            .map_err(|err| io::Error::other(format!("failed to load Glue table: {err}")))?
            .table
            .ok_or_else(|| io::Error::other("Glue get_table response did not include a table"))?;

        let mut parameters = glue_table.parameters().cloned().unwrap_or_default();
        parameters.insert("table_type".to_string(), "ICEBERG".to_string());
        parameters.insert(
            "metadata_location".to_string(),
            staged_metadata_location.to_string(),
        );
        parameters.insert(
            "previous_metadata_location".to_string(),
            current_metadata_location.to_string(),
        );

        let mut table_input = TableInput::builder()
            .name(table_ident.name())
            .set_parameters(Some(parameters))
            .set_storage_descriptor(glue_table.storage_descriptor().cloned())
            .set_partition_keys(Some(glue_table.partition_keys().to_vec()))
            .table_type(glue_table.table_type().unwrap_or("EXTERNAL_TABLE"));
        if let Some(description) = glue_table.description() {
            table_input = table_input.description(description);
        }
        if let Some(owner) = glue_table.owner() {
            table_input = table_input.owner(owner);
        }
        let table_input = table_input
            .build()
            .map_err(|err| io::Error::other(format!("failed to build Glue table input: {err}")))?;

        let mut update_table = glue_client
            .update_table()
            .database_name(catalog_namespace)
            .set_skip_archive(Some(true))
            .table_input(table_input);
        if let Some(catalog_id) = self.glue_catalog_id() {
            update_table = update_table.catalog_id(catalog_id);
        }
        update_table
            .send()
            .await
            .map_err(|err| io::Error::other(format!("failed to update Glue table: {err}")))?;
        Ok(())
    }

    async fn prepare_cdc_stream(
        &self,
        mut stream: SendableRecordBatchStream,
        namespace: &str,
        ctx: &crate::plugins::cdc::SyncContext,
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
                let order_token = orders.value(row).to_string();
                let is_stale = state
                    .get(&key)
                    .map(|seen| seen.as_str() >= order_token.as_str())
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
                delete_batches.push(delete);
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
            Err(_) => Ok(HashMap::new()),
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
                .map(|seen| seen.as_str() < token.as_str())
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
        bytes: bytes::Bytes,
    ) -> Result<String, io::Error> {
        let table_location = self.table_location(namespace).ok_or_else(|| {
            io::Error::other("Iceberg sink requires table_location_prefix for data file writes")
        })?;
        let (bucket, table_prefix) = parse_s3_uri(&table_location)?;
        let digest = hex::encode(md5::compute(filename).0);
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
        Ok(format!("s3://{}/{}", bucket, key))
    }

    async fn plan_cdc_commit(
        &self,
        table: &iceberg::table::Table,
        namespace: &str,
        cdc_ctx: Option<&crate::plugins::cdc::SyncContext>,
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

    async fn glue_catalog(&self) -> Result<GlueCatalog, io::Error> {
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

        let mut props =
            HashMap::from([(GLUE_CATALOG_PROP_WAREHOUSE.to_string(), warehouse.clone())]);
        if let Some(catalog_id) = catalog_id {
            props.insert(GLUE_CATALOG_PROP_CATALOG_ID.to_string(), catalog_id.clone());
        }
        if let Some(region) = region {
            props.insert(AWS_REGION_NAME.to_string(), region.clone());
        }
        GlueCatalogBuilder::default()
            .load("glue", props)
            .await
            .map_err(|err| io::Error::other(err.to_string()))
    }

    async fn glue_client(&self) -> aws_sdk_glue::Client {
        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
        if let Some(region) = self.glue_region() {
            loader = loader.region(aws_sdk_glue::config::Region::new(region.to_string()));
        }
        let config = loader.load().await;
        aws_sdk_glue::Client::new(&config)
    }

    fn glue_catalog_id(&self) -> Option<&str> {
        match &self.config.catalog {
            IcebergCatalogConfig::Glue { catalog_id, .. } => catalog_id.as_deref(),
            IcebergCatalogConfig::Rest { .. }
            | IcebergCatalogConfig::Unity { .. }
            | IcebergCatalogConfig::Polaris { .. } => None,
        }
    }

    fn glue_region(&self) -> Option<&str> {
        match &self.config.catalog {
            IcebergCatalogConfig::Glue { region, .. } => region.as_deref(),
            IcebergCatalogConfig::Rest { .. }
            | IcebergCatalogConfig::Unity { .. }
            | IcebergCatalogConfig::Polaris { .. } => None,
        }
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
            return catalog
                .load_table(&table_ident)
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
            format!(
                "{}/{}",
                prefix.trim_end_matches('/'),
                self.table_name(namespace)
            )
        })
    }
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

fn commit_mode_for_files(files: &[iceberg::spec::DataFile]) -> IcebergCommitMode {
    commit_mode_for_content_types(files.iter().map(|file| file.content_type()))
}

fn commit_mode_for_content_types(
    content_types: impl IntoIterator<Item = DataContentType>,
) -> IcebergCommitMode {
    if content_types
        .into_iter()
        .any(|content_type| content_type != DataContentType::Data)
    {
        IcebergCommitMode::RowDelta
    } else {
        IcebergCommitMode::FastAppend
    }
}

fn generate_snapshot_id(table: &iceberg::table::Table) -> i64 {
    loop {
        let (lhs, rhs) = Uuid::new_v4().as_u64_pair();
        let snapshot_id = (lhs ^ rhs) as i64;
        let snapshot_id = if snapshot_id < 0 {
            -snapshot_id
        } else {
            snapshot_id
        };
        if !table
            .metadata()
            .snapshots()
            .any(|snapshot| snapshot.snapshot_id() == snapshot_id)
        {
            return snapshot_id;
        }
    }
}

fn current_time_millis() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn snapshot_summary_for_files(files: &[iceberg::spec::DataFile]) -> Summary {
    let data_files = files
        .iter()
        .filter(|file| file.content_type() == DataContentType::Data)
        .count();
    let delete_files = files
        .iter()
        .filter(|file| file.content_type() != DataContentType::Data)
        .count();
    let added_records: u64 = files
        .iter()
        .filter(|file| file.content_type() == DataContentType::Data)
        .map(|file| file.record_count())
        .sum();
    let deleted_records: u64 = files
        .iter()
        .filter(|file| file.content_type() != DataContentType::Data)
        .map(|file| file.record_count())
        .sum();
    let mut additional_properties = HashMap::new();
    if data_files > 0 {
        additional_properties.insert("added-data-files".to_string(), data_files.to_string());
        additional_properties.insert("added-records".to_string(), added_records.to_string());
    }
    if delete_files > 0 {
        additional_properties.insert("added-delete-files".to_string(), delete_files.to_string());
        additional_properties.insert("deleted-records".to_string(), deleted_records.to_string());
    }
    let operation = match (data_files > 0, delete_files > 0) {
        (true, false) => Operation::Append,
        (false, true) => Operation::Delete,
        (true, true) => Operation::Overwrite,
        (false, false) => Operation::Append,
    };
    Summary {
        operation,
        additional_properties,
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

fn business_key_for_row(
    batch: &RecordBatch,
    business_key_columns: &[String],
    row: usize,
) -> Result<String, io::Error> {
    let mut values = Vec::with_capacity(business_key_columns.len());
    for column in business_key_columns {
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
        values.push(format!("{}={}", column, value));
    }
    Ok(values.join("|"))
}

fn iceberg_schema_from_output_metadata(
    namespace: &str,
    metadata: &OutputMetadata,
) -> Result<Schema, io::Error> {
    let mut fields = Vec::new();
    for (_, field) in metadata.child_fields() {
        fields.push(Arc::new(field_to_nested_field(
            namespace,
            field,
            Vec::new(),
        )?));
    }
    Schema::builder()
        .with_schema_id(0)
        .with_fields(fields)
        .build()
        .map_err(|err| io::Error::other(err.to_string()))
}

fn field_to_nested_field(
    namespace: &str,
    metadata: &OutputMetadata,
    mut path: Vec<String>,
) -> Result<NestedField, io::Error> {
    path.push(metadata.out_field_name().to_string());
    let field_id = if metadata.field_id() != 0 {
        metadata.field_id()
    } else {
        crate::lineage::deterministic_field_id(namespace, &path)
    };
    let field_type = match metadata.determined_type() {
        SkipprDataType::Record => {
            let mut children = Vec::new();
            for (_, child) in metadata.child_fields() {
                children.push(Arc::new(field_to_nested_field(
                    namespace,
                    child,
                    path.clone(),
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
                    .map(|(_, child)| field_to_nested_field(namespace, child, path.clone()))
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
            Type::List(ListType::new(Arc::new(NestedField::list_element(
                crate::lineage::deterministic_field_id(namespace, &element_path),
                element_type,
                true,
            ))))
        }
        SkipprDataType::Map => {
            let mut key_path = path.clone();
            key_path.push("key".to_string());
            let mut value_path = path.clone();
            value_path.push("value".to_string());
            Type::Map(MapType::new(
                Arc::new(NestedField::map_key_element(
                    crate::lineage::deterministic_field_id(namespace, &key_path),
                    Type::Primitive(PrimitiveType::String),
                )),
                Arc::new(NestedField::map_value_element(
                    crate::lineage::deterministic_field_id(namespace, &value_path),
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

fn parse_s3_uri(uri: &str) -> Result<(String, String), io::Error> {
    let without_scheme = uri
        .strip_prefix("s3://")
        .ok_or_else(|| io::Error::other(format!("expected s3:// URI, got '{}'", uri)))?;
    let (bucket, key) = without_scheme
        .split_once('/')
        .ok_or_else(|| io::Error::other(format!("expected s3://bucket/key URI, got '{}'", uri)))?;
    Ok((bucket.to_string(), key.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn output_metadata(value: serde_json::Value) -> OutputMetadata {
        serde_json::from_value(value).unwrap()
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

        let field = field_to_nested_field("orders", &metadata, Vec::new()).unwrap();
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

        let array_field = field_to_nested_field("orders", &array_metadata, Vec::new()).unwrap();
        let map_field = field_to_nested_field("orders", &map_metadata, Vec::new()).unwrap();

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
    fn equality_delete_files_use_row_delta_commit_mode() {
        assert_eq!(
            commit_mode_for_content_types([DataContentType::Data]),
            IcebergCommitMode::FastAppend
        );
        assert_eq!(
            commit_mode_for_content_types([
                DataContentType::Data,
                DataContentType::EqualityDeletes
            ]),
            IcebergCommitMode::RowDelta
        );
    }
}
