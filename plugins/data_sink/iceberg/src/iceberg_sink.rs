use std::collections::{BTreeMap, HashMap};
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll as TaskPoll};
use std::time::Duration;

use arrow::array::{Array, BooleanArray, RecordBatch, StringArray};
use arrow::compute::filter_record_batch;
use arrow::datatypes::SchemaRef;
use arrow::util::display::array_value_to_string;
use async_trait::async_trait;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client as S3Client;
use datafusion::error::DataFusionError;
use datafusion::execution::SendableRecordBatchStream;
use datafusion::physical_plan::RecordBatchStream;
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
use serde_derive::Deserialize;
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::helpers::configuration::DataSinkPluginConfig;
use skippr_runtime_sdk::discover::{OutputMetadata, SkipprDataType};
use skippr_runtime_sdk::plugins::cdc::EffectiveGuarantee;
use skippr_runtime_sdk::plugins::{DataSink, SchemaSink};
use skippr_runtime_sdk::protocol::{RuntimeBinding, RuntimeExecutionContext};
use skippr_runtime_sdk::sink_compat::BufferChunker;

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
        cdc_ctx: Option<&skippr_runtime_sdk::plugins::cdc::SyncContext>,
    ) -> Result<(), io::Error> {
        let stream = match cdc_ctx {
            Some(ctx) => super::cdc_encode::augment_stream_with_cdc_columns(stream, &ctx.part_meta),
            None => stream,
        };
        self.native_append(stream, filename, cdc_ctx).await
    }

    fn capability(&self) -> Option<&'static skippr_runtime_sdk::plugins::cdc::SinkCapability> {
        Some(&skippr_runtime_sdk::plugins::cdc::sink_capabilities::ICEBERG)
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
        cdc_ctx: Option<&skippr_runtime_sdk::plugins::cdc::SyncContext>,
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
    append_cdc_encoded_fields(namespace, &mut fields);
    Schema::builder()
        .with_schema_id(0)
        .with_fields(fields)
        .build()
        .map_err(|err| io::Error::other(err.to_string()))
}

fn append_cdc_encoded_fields(namespace: &str, fields: &mut Vec<Arc<NestedField>>) {
    for name in ["_skippr_mutation", "_skippr_order_token"] {
        if fields.iter().any(|field| field.name == name) {
            continue;
        }
        fields.push(Arc::new(NestedField::new(
            crate::lineage::deterministic_field_id(namespace, &[name.to_string()]),
            name,
            Type::Primitive(PrimitiveType::String),
            true,
        )));
    }
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
}
