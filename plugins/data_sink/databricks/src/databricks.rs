use skippr_runtime_sdk::SkipprConfig;
use std::collections::HashMap;

use super::parquet_util::serialize_to_parquet;
use crate::helpers::configuration::DataSinkPluginConfig;
use async_trait::async_trait;
use dashmap::DashSet;
use datafusion::execution::SendableRecordBatchStream;
use futures::StreamExt;
use once_cell::sync::Lazy;
use reqwest::Client;
use serde_derive::Deserialize;
use skippr_runtime_sdk::discover::{OutputMetadata, SkipprDataType};
use skippr_runtime_sdk::plugins::alter_ddl::{
    dest_table_name, missing_published_columns, published_columns_folded,
    require_published_columns_live, DestPublishCache, IdentCase, InstalledDestSchema,
    PublishedBatchColumns, PublishedColumn,
};
use skippr_runtime_sdk::plugins::DataSink;
use skippr_runtime_sdk::sink_compat::BufferChunker;
use tracing::{error, info};

static CDC_DDL_ENSURED: Lazy<DashSet<String>> = Lazy::new(DashSet::new);
const DATABRICKS_BULK_CDC_BLOCKER: &str = "multi-table CDC transactions require DBR 18+ \
    Unity Catalog managed tables with catalog commits enabled, which this connector cannot \
    currently require for every configured target";

pub struct DatabricksCdcBackend;

impl super::cdc_apply::CdcApplyBackend for DatabricksCdcBackend {
    const ORDER_TOKEN_TYPE: &'static str = "BINARY";

    fn binary_literal(hex: &str) -> String {
        format!("X'{hex}'")
    }
}

#[derive(Debug, Deserialize, SkipprConfig, Clone)]
pub struct DataSinkDatabricksPluginConfig {
    #[serde(default)]
    pub workspace_url: String,
    #[serde(default)]
    #[skippr(secret)]
    pub token: String,
    pub warehouse_id: Option<String>,
    pub catalog: Option<String>,
    pub schema: Option<String>,
    pub table: Option<String>,
    pub delta_table_uri: Option<String>,
    #[serde(default)]
    pub storage_options: Option<HashMap<String, String>>,
}

impl TryFrom<DataSinkPluginConfig> for DataSinkDatabricksPluginConfig {
    type Error = String;

    fn try_from(entry: DataSinkPluginConfig) -> Result<Self, Self::Error> {
        entry.decode_for_plugin("Databricks")
    }
}

pub struct DataSinkDatabricksPlugin {
    client: Client,
    config: DataSinkDatabricksPluginConfig,
    order_fields: Vec<String>,
    installed: tokio::sync::RwLock<InstalledDestSchema>,
    publish_cache: DestPublishCache,
}

skippr_runtime_sdk::declare_sink_spec!(
    DatabricksSinkSpec,
    DataSinkDatabricksPlugin,
    skippr_runtime_sdk::plugins::cdc::sink_capabilities::DATABRICKS,
    skippr_runtime_sdk::plugins::FinalStateIdempotentApply
);

#[async_trait]
impl DataSink for DataSinkDatabricksPlugin {
    async fn sync(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
        cdc_ctx: Option<&skippr_runtime_sdk::plugins::cdc::SyncContext>,
    ) -> Result<(), std::io::Error> {
        let namespace = BufferChunker::decode_file_namespace(&filename);
        self.publish_namespace(&namespace).await?;
        match cdc_ctx {
            Some(ctx) => self.sync_cdc(stream, filename, ctx).await,
            None => self.sync_append(stream, filename).await,
        }
    }

    async fn sync_grouped(
        &self,
        mut reader: skippr_runtime_sdk::plugins::GroupedBatchReader,
        ctx: skippr_runtime_sdk::plugins::GroupedSinkWriteContext<'_>,
    ) -> Result<skippr_runtime_sdk::plugins::SinkWriteOutcome, std::io::Error> {
        let schema = reader.schema();
        while let Some(chunk) = reader.next_chunk().await? {
            let chunk_cdc = ctx.chunk_cdc_context(&chunk)?;
            let chunk_ctx = ctx.chunk_sink_write_context_with_cdc(
                chunk.chunk_index,
                chunk.chunk_index == 0 && chunk.final_chunk,
                chunk_cdc.as_ref(),
            );
            self.sync(
                chunk.into_stream(schema.clone()),
                chunk_ctx.filename,
                chunk_ctx.cdc_ctx,
            )
            .await?;
        }
        Ok(skippr_runtime_sdk::plugins::SinkWriteOutcome::Applied)
    }

    fn capability(&self) -> &'static skippr_runtime_sdk::plugins::cdc::SinkCapability {
        &skippr_runtime_sdk::plugins::cdc::sink_capabilities::DATABRICKS
    }

    fn installed_dest_schema(&self) -> Option<&tokio::sync::RwLock<InstalledDestSchema>> {
        Some(&self.installed)
    }
}

impl DataSinkDatabricksPlugin {
    pub async fn new_with_config(
        _buffer_name: String,
        config: DataSinkDatabricksPluginConfig,
        order_fields: Vec<String>,
    ) -> Self {
        Self {
            client: Client::new(),
            config,
            order_fields,
            installed: tokio::sync::RwLock::new(InstalledDestSchema::default()),
            publish_cache: DestPublishCache::default(),
        }
    }

    fn catalog(&self) -> &str {
        self.config.catalog.as_deref().unwrap_or("main")
    }

    fn schema(&self) -> &str {
        self.config.schema.as_deref().unwrap_or("default")
    }

    fn dest_table(&self, namespace: &str) -> String {
        self.config
            .table
            .clone()
            .unwrap_or_else(|| dest_table_name(namespace))
    }

    fn quote_ident(name: &str) -> String {
        format!("`{}`", name.replace('`', "``"))
    }

    fn fq_table(&self, namespace: &str) -> String {
        format!(
            "{}.{}.{}",
            Self::quote_ident(self.catalog()),
            Self::quote_ident(self.schema()),
            Self::quote_ident(&self.dest_table(namespace))
        )
    }

    /// The shared CDC backend and `tombstone_table_name` build double-quoted SQL.
    fn cdc_fq_table(&self, namespace: &str) -> String {
        format!(
            "\"{}\".\"{}\".\"{}\"",
            self.catalog(),
            self.schema(),
            self.dest_table(namespace)
        )
    }

    fn sql_type(ty: &SkipprDataType) -> Result<&'static str, std::io::Error> {
        Ok(match ty {
            SkipprDataType::Boolean => "BOOLEAN",
            SkipprDataType::Byte | SkipprDataType::Short | SkipprDataType::Integer => "INT",
            SkipprDataType::Long => "BIGINT",
            SkipprDataType::Float => "FLOAT",
            SkipprDataType::Double => "DOUBLE",
            SkipprDataType::String | SkipprDataType::Uuid | SkipprDataType::Json => "STRING",
            SkipprDataType::Binary | SkipprDataType::Fixed => "BINARY",
            SkipprDataType::Date => "DATE",
            SkipprDataType::Timestamp | SkipprDataType::TimestampMilli => "TIMESTAMP",
            other => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("Databricks publish has no SQL type for {other}"),
                ))
            }
        })
    }

    /// CREATE when the table is missing. Otherwise one ADD COLUMNS for names
    /// the live table lacks; existing columns are left to `apply_alter`.
    fn publish_sql(
        fq_table: &str,
        metadata: &OutputMetadata,
        existing: &[String],
    ) -> Result<Option<String>, std::io::Error> {
        let columns = published_columns_folded(metadata, IdentCase::Lower)?;
        if existing.is_empty() {
            return Ok(Some(format!(
                "CREATE TABLE IF NOT EXISTS {} ({})",
                fq_table,
                Self::column_defs(&columns)?
            )));
        }
        let missing = missing_published_columns(&columns, existing, IdentCase::Lower);
        if missing.is_empty() {
            return Ok(None);
        }
        Ok(Some(format!(
            "ALTER TABLE {} ADD COLUMNS ({})",
            fq_table,
            Self::column_defs(missing)?
        )))
    }

    fn column_defs<'a>(
        columns: impl IntoIterator<Item = &'a PublishedColumn>,
    ) -> Result<String, std::io::Error> {
        let defs = columns
            .into_iter()
            .map(|column| {
                Ok(format!(
                    "{} {}",
                    Self::quote_ident(&IdentCase::Lower.fold(&column.name)),
                    Self::sql_type(&column.ty)?
                ))
            })
            .collect::<Result<Vec<_>, std::io::Error>>()?;
        Ok(defs.join(", "))
    }

    fn delta_type(ty: &SkipprDataType) -> Result<deltalake::kernel::DataType, std::io::Error> {
        use deltalake::kernel::DataType as DeltaType;
        Ok(match ty {
            SkipprDataType::Boolean => DeltaType::BOOLEAN,
            SkipprDataType::Byte | SkipprDataType::Short | SkipprDataType::Integer => {
                DeltaType::INTEGER
            }
            SkipprDataType::Long => DeltaType::LONG,
            SkipprDataType::Float => DeltaType::FLOAT,
            SkipprDataType::Double => DeltaType::DOUBLE,
            SkipprDataType::String | SkipprDataType::Uuid | SkipprDataType::Json => {
                DeltaType::STRING
            }
            SkipprDataType::Binary | SkipprDataType::Fixed => DeltaType::BINARY,
            SkipprDataType::Date => DeltaType::DATE,
            SkipprDataType::Timestamp | SkipprDataType::TimestampMilli => DeltaType::TIMESTAMP,
            other => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("Databricks Delta publish has no type for {other}"),
                ))
            }
        })
    }

    fn delta_fields_from_metadata(
        metadata: &OutputMetadata,
    ) -> Result<Vec<deltalake::kernel::StructField>, std::io::Error> {
        published_columns_folded(metadata, IdentCase::Lower)?
            .into_iter()
            .map(|column| {
                Ok(deltalake::kernel::StructField::new(
                    column.name,
                    Self::delta_type(&column.ty)?,
                    true,
                ))
            })
            .collect()
    }

    /// Published fields the live Delta schema lacks, matched case-insensitively
    /// like Spark. A live field whose type differs from published fails closed.
    fn delta_missing_fields(
        table: &deltalake::DeltaTable,
        expected: &[deltalake::kernel::StructField],
    ) -> Result<Vec<deltalake::kernel::StructField>, std::io::Error> {
        let schema = table
            .snapshot()
            .map_err(|err| std::io::Error::other(format!("Delta snapshot: {err}")))?
            .schema();
        let live: Vec<(String, String)> = schema
            .fields()
            .map(|field| {
                (
                    IdentCase::Lower.fold(field.name()),
                    field.data_type().to_string(),
                )
            })
            .collect();
        let mut missing = Vec::new();
        for field in expected {
            let name = IdentCase::Lower.fold(field.name());
            let ty = field.data_type().to_string();
            match live.iter().find(|(live_name, _)| *live_name == name) {
                None => missing.push(field.clone()),
                Some((_, live_ty)) if *live_ty != ty => {
                    return Err(std::io::Error::other(format!(
                        "Delta table column '{}' type '{live_ty}' does not match published '{ty}'",
                        field.name()
                    )));
                }
                Some(_) => {}
            }
        }
        Ok(missing)
    }

    async fn ensure_delta_table(
        &self,
        uri: &str,
        metadata: &OutputMetadata,
    ) -> Result<(), std::io::Error> {
        let fields = Self::delta_fields_from_metadata(metadata)?;
        let storage_opts = self.config.storage_options.clone().unwrap_or_default();
        let table = deltalake::operations::create::CreateBuilder::new()
            .with_location(uri)
            .with_storage_options(storage_opts)
            .with_columns(fields.clone())
            .with_save_mode(deltalake::protocol::SaveMode::Ignore)
            .await
            .map_err(|err| {
                std::io::Error::other(format!("Delta create from published metadata: {err}"))
            })?;
        let missing = Self::delta_missing_fields(&table, &fields)?;
        if missing.is_empty() {
            return Ok(());
        }
        let table = table
            .add_columns()
            .with_fields(missing)
            .await
            .map_err(|err| std::io::Error::other(format!("Delta add published columns: {err}")))?;
        match Self::delta_missing_fields(&table, &fields)?.first() {
            Some(field) => Err(std::io::Error::other(format!(
                "Delta table is missing published column '{}' after publish",
                field.name()
            ))),
            None => Ok(()),
        }
    }

    async fn publish_namespace(&self, namespace: &str) -> Result<(), std::io::Error> {
        let (version, metadata) = self.installed.read().await.get(namespace)?;
        self.publish_cache
            .publish(namespace, version, || async {
                if let Some(uri) = self.config.delta_table_uri.as_deref() {
                    return self.ensure_delta_table(uri, &metadata).await;
                }
                let existing = self.table_columns(namespace).await?;
                if let Some(sql) =
                    Self::publish_sql(&self.fq_table(namespace), &metadata, &existing)?
                {
                    self.execute_sql(&sql).await?;
                }
                require_published_columns_live(
                    &published_columns_folded(&metadata, IdentCase::Lower)?,
                    &self.table_columns(namespace).await?,
                    IdentCase::Lower,
                )
            })
            .await
    }

    async fn batch_columns(
        &self,
        namespace: &str,
        batch: &datafusion::arrow::datatypes::Schema,
    ) -> Result<PublishedBatchColumns, std::io::Error> {
        self.installed.read().await.batch_columns(
            namespace,
            batch,
            IdentCase::Lower,
            Self::sql_type,
        )
    }

    async fn sync_append(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
    ) -> Result<(), std::io::Error> {
        let namespace = BufferChunker::decode_file_namespace(&filename);
        self.batch_columns(&namespace, &stream.schema()).await?;
        if self.config.delta_table_uri.is_some() {
            self.sync_delta(stream).await
        } else {
            self.sync_copy(stream, filename).await
        }
    }

    async fn sync_copy(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
    ) -> Result<(), std::io::Error> {
        use skippr_runtime_sdk::metrics::counters;
        counters::inc_uploads_in_flight();

        let parquet_bytes = serialize_to_parquet(stream, &self.order_fields)
            .await
            .map_err(|e| {
                counters::dec_uploads_in_flight();
                e
            })?;

        let upload_path = format!(
            "/Volumes/{}/{}/staging/{}",
            self.catalog(),
            self.schema(),
            filename,
        );

        let url = format!(
            "{}/api/2.0/fs/files{}",
            self.config.workspace_url.trim_end_matches('/'),
            upload_path,
        );

        self.client
            .put(&url)
            .bearer_auth(&self.config.token)
            .header("Content-Type", "application/octet-stream")
            .body(parquet_bytes.bytes.to_vec())
            .send()
            .await
            .and_then(|response| response.error_for_status())
            .map_err(|e| {
                counters::dec_uploads_in_flight();
                std::io::Error::other(e.to_string())
            })?;

        if self.config.warehouse_id.is_some() {
            let namespace = BufferChunker::decode_file_namespace(&filename);
            let sql = format!(
                "COPY INTO {} FROM '{}' FILEFORMAT = PARQUET",
                self.fq_table(&namespace),
                upload_path.replace('\'', "''"),
            );
            self.execute_sql(&sql).await.map_err(|e| {
                counters::dec_uploads_in_flight();
                e
            })?;
        }

        info!("Databricks: uploaded {}", upload_path);
        counters::dec_uploads_in_flight();
        Ok(())
    }

    async fn sync_delta(&self, stream: SendableRecordBatchStream) -> Result<(), std::io::Error> {
        use skippr_runtime_sdk::metrics::counters;
        counters::inc_uploads_in_flight();

        let delta_uri = self.config.delta_table_uri.as_deref().unwrap();
        let storage_opts = self.config.storage_options.clone().unwrap_or_default();
        let table_url = deltalake::ensure_table_uri(delta_uri)
            .map_err(|e| std::io::Error::other(format!("Delta URI: {}", e)))?;

        let parquet_bytes = serialize_to_parquet(stream, &self.order_fields)
            .await
            .map_err(|e| {
                counters::dec_uploads_in_flight();
                e
            })?;
        let total_rows = parquet_bytes.num_rows as u64;

        if total_rows == 0 {
            counters::dec_uploads_in_flight();
            return Ok(());
        }

        let tmp = tempfile::NamedTempFile::new().map_err(|e| {
            counters::dec_uploads_in_flight();
            std::io::Error::other(format!("temp file: {}", e))
        })?;
        std::fs::write(tmp.path(), &parquet_bytes.bytes).map_err(|e| {
            counters::dec_uploads_in_flight();
            std::io::Error::other(format!("temp write: {}", e))
        })?;

        let ctx = deltalake::datafusion::prelude::SessionContext::new();
        let df = ctx
            .read_parquet(
                tmp.path().to_str().unwrap(),
                deltalake::datafusion::prelude::ParquetReadOptions::default(),
            )
            .await
            .map_err(|e| {
                counters::dec_uploads_in_flight();
                std::io::Error::other(format!("Delta read parquet: {}", e))
            })?;

        let delta_batches: Vec<deltalake::arrow::record_batch::RecordBatch> =
            df.collect().await.map_err(|e| {
                counters::dec_uploads_in_flight();
                std::io::Error::other(format!("Delta collect: {}", e))
            })?;

        let table =
            deltalake::DeltaTable::try_from_url_with_storage_options(table_url, storage_opts)
                .await
                .map_err(|e| std::io::Error::other(format!("Delta init: {}", e)))?;
        table
            .write(delta_batches)
            .with_save_mode(deltalake::protocol::SaveMode::Append)
            .await
            .map_err(|e| std::io::Error::other(format!("Delta write: {}", e)))?;

        counters::add_parquet_rows(total_rows);
        counters::add_upload(1);
        counters::dec_uploads_in_flight();
        info!("Delta Lake: wrote {} rows to {}", total_rows, delta_uri);
        Ok(())
    }

    /// Runs one statement and fails closed unless it reaches `SUCCEEDED`.
    async fn execute_statement(&self, sql: &str) -> Result<serde_json::Value, std::io::Error> {
        let warehouse_id = self
            .config
            .warehouse_id
            .as_deref()
            .ok_or_else(|| std::io::Error::other("Databricks SQL requires warehouse_id"))?;
        let url = format!(
            "{}/api/2.0/sql/statements",
            self.config.workspace_url.trim_end_matches('/')
        );
        let mut body = statement_response(
            self.client
                .post(&url)
                .bearer_auth(&self.config.token)
                .json(&serde_json::json!({
                    "warehouse_id": warehouse_id,
                    "statement": sql,
                    "wait_timeout": "50s",
                }))
                .send()
                .await,
        )
        .await?;
        for _ in 0..DATABRICKS_STATEMENT_POLLS {
            if !statement_running(&body) {
                break;
            }
            let id = body
                .get("statement_id")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| std::io::Error::other("Databricks running statement has no id"))?;
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            body = statement_response(
                self.client
                    .get(format!("{url}/{id}"))
                    .bearer_auth(&self.config.token)
                    .send()
                    .await,
            )
            .await?;
        }
        statement_succeeded(body)
    }

    async fn execute_sql(&self, sql: &str) -> Result<(), std::io::Error> {
        self.execute_statement(sql).await.map(|_| ())
    }

    async fn table_columns(&self, namespace: &str) -> Result<Vec<String>, std::io::Error> {
        let sql = format!(
            "SELECT column_name FROM {}.information_schema.columns WHERE table_schema = '{}' AND table_name = '{}'",
            Self::quote_ident(self.catalog()),
            self.schema().replace('\'', "''"),
            self.dest_table(namespace).replace('\'', "''"),
        );
        statement_first_column(&self.execute_statement(&sql).await?)
    }

    /// Databricks multi-table transactions require DBR 18+ plus Unity Catalog
    /// managed tables with catalog commits enabled. This connector also
    /// accepts ordinary/external Delta targets and cannot currently require or
    /// verify those prerequisites. Keep the existing replay-guarded path until
    /// the capability can be negotiated; bulk staging only one side would
    /// weaken delete/replay safety.
    async fn sync_cdc(
        &self,
        mut stream: SendableRecordBatchStream,
        filename: String,
        ctx: &skippr_runtime_sdk::plugins::cdc::SyncContext,
    ) -> Result<(), std::io::Error> {
        tracing::debug!(
            target: "databricks",
            blocker = DATABRICKS_BULK_CDC_BLOCKER,
            "bulk CDC staging deferred"
        );
        use super::cdc_apply::{
            append_record_batch_to_cdc_apply, collapse_apply_batch, ddl_add_order_token_column,
            ddl_create_tombstone_table, delete_if_newer_sql, format_cdc_apply_value,
            tombstone_table_name, upsert_if_newer_sql, CdcApplyBatch, CdcApplyColumn,
            CdcApplyMutation,
        };
        use skippr_runtime_sdk::metrics::counters;

        let contract = match ctx.contract.as_ref() {
            Some(c) if !c.business_key_columns.is_empty() => c,
            _ => {
                info!(target: "databricks", "CDC context without contract or business keys; falling back to append");
                return self.sync_append(stream, filename).await;
            }
        };

        let namespace = BufferChunker::decode_file_namespace(&filename);
        let col_defs = self.batch_columns(&namespace, &stream.schema()).await?;
        let key_types = col_defs.key_types(&contract.business_key_columns)?;
        let business_key_columns: Vec<String> =
            key_types.iter().map(|(name, _)| name.clone()).collect();
        let fq_table = self.cdc_fq_table(&namespace);
        counters::inc_uploads_in_flight();

        if CDC_DDL_ENSURED.insert(fq_table.clone()) {
            let order_col_ddl = ddl_add_order_token_column::<DatabricksCdcBackend>(&fq_table);
            if let Err(e) = self.execute_sql(&order_col_ddl).await {
                let msg = e.to_string();
                if !msg.contains("already exists") {
                    CDC_DDL_ENSURED.remove(&fq_table);
                    counters::dec_uploads_in_flight();
                    return Err(e);
                }
            }

            let tombstone_tbl = tombstone_table_name(&fq_table);
            let tombstone_ddl =
                ddl_create_tombstone_table::<DatabricksCdcBackend>(&tombstone_tbl, &key_types);
            if let Err(e) = self.execute_sql(&tombstone_ddl).await {
                let msg = e.to_string();
                if !msg.contains("already exists") {
                    CDC_DDL_ENSURED.remove(&fq_table);
                    counters::dec_uploads_in_flight();
                    return Err(e);
                }
            }

            info!(target: "databricks", "CDC DDL applied for {}", fq_table);
        }

        let tombstone_table = tombstone_table_name(&fq_table);

        let bk_names_quoted: Vec<String> = business_key_columns
            .iter()
            .map(|bk| format!("\"{}\"", bk))
            .collect();
        let bk_types: Vec<String> = key_types.iter().map(|(_, ty)| ty.to_string()).collect();
        let col_names_quoted: Vec<String> = col_defs
            .columns()
            .iter()
            .map(|(name, _)| format!("\"{}\"", name))
            .collect();

        let mut apply_batch = CdcApplyBatch {
            columns: col_defs
                .columns()
                .iter()
                .map(|(name, target_type)| CdcApplyColumn {
                    name: name.clone(),
                    target_type: (*target_type).to_string(),
                })
                .collect(),
            business_key_columns: business_key_columns.clone(),
            rows: Vec::new(),
        };
        let mut row_offset = 0usize;
        let ties = ctx.decoded_ties().map_err(|e| {
            counters::dec_uploads_in_flight();
            std::io::Error::other(e)
        })?;
        while let Some(batch_result) = stream.next().await {
            let batch = batch_result.map_err(|e| {
                counters::dec_uploads_in_flight();
                std::io::Error::other(format!("stream error: {}", e))
            })?;
            let num_rows = batch.num_rows();
            if num_rows == 0 {
                continue;
            }
            append_record_batch_to_cdc_apply(
                &mut apply_batch,
                &batch,
                &ctx.part_meta.rows,
                &ties,
                row_offset,
            )
            .map_err(|e| {
                counters::dec_uploads_in_flight();
                std::io::Error::from(e)
            })?;
            row_offset += num_rows;
        }
        let apply_batch = collapse_apply_batch(&apply_batch);
        let total_rows = apply_batch.rows.len();
        for row in &apply_batch.rows {
            let incoming = skippr_cdc::StoredWinner {
                order: row.metadata.order_token.clone(),
                tie: row.metadata.tie,
            };
            match row.metadata.mutation {
                CdcApplyMutation::Upsert => {
                    let mut all_names = col_names_quoted.clone();
                    all_names.push("\"_skippr_order_token\"".to_string());
                    all_names.push("\"_skippr_tie_seq\"".to_string());
                    all_names.push("\"_skippr_tie_ordinal\"".to_string());
                    all_names.push("\"_skippr_tie_row\"".to_string());
                    let mut all_values: Vec<String> = row
                        .values
                        .iter()
                        .map(format_cdc_apply_value::<DatabricksCdcBackend>)
                        .collect();
                    all_values.push(format!("X'{}'", hex::encode(&incoming.order)));
                    all_values.push(incoming.tie.seq.to_string());
                    all_values.push(incoming.tie.ordinal.to_string());
                    all_values.push(incoming.tie.row.to_string());
                    let sql = upsert_if_newer_sql::<DatabricksCdcBackend>(
                        &fq_table,
                        &tombstone_table,
                        &all_names,
                        &all_values,
                        &bk_names_quoted,
                        &incoming,
                    );
                    self.execute_sql(&sql).await.map_err(|e| {
                        error!("CDC upsert failed for {}: {}", fq_table, e);
                        counters::dec_uploads_in_flight();
                        e
                    })?;
                }
                CdcApplyMutation::Delete => {
                    let bk_values: Vec<String> = business_key_columns
                        .iter()
                        .map(|bk| {
                            let idx = apply_batch
                                .columns
                                .iter()
                                .position(|column| column.name == *bk)
                                .unwrap_or(0);
                            format_cdc_apply_value::<DatabricksCdcBackend>(&row.values[idx])
                        })
                        .collect();
                    let sql = delete_if_newer_sql::<DatabricksCdcBackend>(
                        &fq_table,
                        &tombstone_table,
                        &bk_names_quoted,
                        &bk_values,
                        &bk_types,
                        &incoming,
                    );
                    self.execute_sql(&sql).await.map_err(|e| {
                        error!("CDC delete failed for {}: {}", fq_table, e);
                        counters::dec_uploads_in_flight();
                        e
                    })?;
                }
            }
        }
        if total_rows > 0 {
            counters::add_parquet_rows(total_rows as u64);
            info!("CDC applied {} collapsed rows to {}", total_rows, fq_table);
        }

        counters::add_upload(1);
        counters::dec_uploads_in_flight();
        info!(
            "Databricks CDC sync complete: {} total rows into {}",
            total_rows, fq_table
        );
        Ok(())
    }
}

/// Ceiling on one-second status polls after the 50s inline wait.
const DATABRICKS_STATEMENT_POLLS: u32 = 600;

async fn statement_response(
    response: reqwest::Result<reqwest::Response>,
) -> Result<serde_json::Value, std::io::Error> {
    response
        .and_then(|response| response.error_for_status())
        .map_err(|e| std::io::Error::other(e.to_string()))?
        .json()
        .await
        .map_err(|e| std::io::Error::other(format!("Databricks statement response: {e}")))
}

fn statement_running(body: &serde_json::Value) -> bool {
    matches!(
        body.pointer("/status/state")
            .and_then(serde_json::Value::as_str),
        Some("PENDING" | "RUNNING")
    )
}

fn statement_succeeded(body: serde_json::Value) -> Result<serde_json::Value, std::io::Error> {
    match body
        .pointer("/status/state")
        .and_then(serde_json::Value::as_str)
    {
        Some("SUCCEEDED") => Ok(body),
        state => Err(std::io::Error::other(format!(
            "Databricks statement {}: {}",
            state.unwrap_or("has no status"),
            body.pointer("/status/error/message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
        ))),
    }
}

/// First cell of each `JSON_ARRAY` row. No `data_array` is only valid for zero rows.
fn statement_first_column(body: &serde_json::Value) -> Result<Vec<String>, std::io::Error> {
    let Some(rows) = body.pointer("/result/data_array") else {
        return match body
            .pointer("/manifest/total_row_count")
            .and_then(serde_json::Value::as_u64)
        {
            Some(0) => Ok(Vec::new()),
            _ => Err(std::io::Error::other(format!(
                "Databricks result has no data_array: {body}"
            ))),
        };
    };
    let rows = rows.as_array().ok_or_else(|| {
        std::io::Error::other(format!("Databricks data_array is not an array: {rows}"))
    })?;
    let total = body
        .pointer("/manifest/total_row_count")
        .and_then(serde_json::Value::as_u64);
    if total != Some(rows.len() as u64) {
        return Err(std::io::Error::other(format!(
            "Databricks result has {} inline rows of {total:?}; chunked results are not read",
            rows.len()
        )));
    }
    rows.iter()
        .map(|row| {
            row.get(0)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| {
                    std::io::Error::other(format!("Databricks row has no text cell: {row}"))
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::DATABRICKS_BULK_CDC_BLOCKER;

    #[test]
    fn bulk_cdc_blocker_records_required_transaction_prerequisites() {
        assert!(DATABRICKS_BULK_CDC_BLOCKER.contains("DBR 18+"));
        assert!(DATABRICKS_BULK_CDC_BLOCKER.contains("catalog commits"));
        assert!(DATABRICKS_BULK_CDC_BLOCKER.contains("every configured target"));
    }

    fn output_with(
        columns: &[(&str, i32, skippr_runtime_sdk::discover::SkipprDataType)],
    ) -> skippr_runtime_sdk::discover::OutputMetadata {
        let mut fields = serde_json::Map::new();
        for (name, id, ty) in columns {
            fields.insert(
                (*name).to_string(),
                serde_json::json!({
                    "out_field_name": name,
                    "source_field_name": name,
                    "determined_type": ty.as_str(),
                    "determined_type_values": "",
                    "field_id": id,
                    "nullable": true,
                    "fields": {}
                }),
            );
        }
        serde_json::from_value(serde_json::json!({
            "out_field_name": "",
            "source_field_name": "",
            "determined_type": "Unknown",
            "determined_type_values": "",
            "nullable": true,
            "fields": fields
        }))
        .expect("output metadata")
    }

    #[test]
    fn publish_sql_creates_then_adds_only_missing_columns_with_backticks() {
        use skippr_runtime_sdk::discover::SkipprDataType;
        let metadata = output_with(&[
            ("id", 1, SkipprDataType::Long),
            ("Region", 2, SkipprDataType::String),
        ]);
        let table = "`main`.`default`.`trips`";
        assert_eq!(
            super::DataSinkDatabricksPlugin::publish_sql(table, &metadata, &[]).unwrap(),
            Some(
                "CREATE TABLE IF NOT EXISTS `main`.`default`.`trips` (`id` BIGINT, `region` STRING)"
                    .to_string()
            )
        );
        assert_eq!(
            super::DataSinkDatabricksPlugin::publish_sql(table, &metadata, &["ID".to_string()])
                .unwrap(),
            Some("ALTER TABLE `main`.`default`.`trips` ADD COLUMNS (`region` STRING)".to_string())
        );
        assert_eq!(
            super::DataSinkDatabricksPlugin::publish_sql(
                table,
                &metadata,
                &["id".to_string(), "region".to_string()]
            )
            .unwrap(),
            None
        );
        let err = super::DataSinkDatabricksPlugin::sql_type(&SkipprDataType::Record).unwrap_err();
        assert!(err.to_string().contains("no SQL type"), "{err}");
        let fields =
            super::DataSinkDatabricksPlugin::delta_fields_from_metadata(&metadata).unwrap();
        let names: Vec<String> = fields
            .iter()
            .map(|field| field.name().to_string())
            .collect();
        assert_eq!(names, vec!["id".to_string(), "Region".to_string()]);
        let delta_err =
            super::DataSinkDatabricksPlugin::delta_type(&SkipprDataType::Record).unwrap_err();
        assert!(delta_err.to_string().contains("no type"), "{delta_err}");
    }

    #[test]
    fn statement_result_fails_closed_unless_succeeded() {
        let ok = serde_json::json!({
            "status": {"state": "SUCCEEDED"},
            "manifest": {"total_row_count": 2},
            "result": {"data_array": [["id"], ["region"]]}
        });
        let chunked = serde_json::json!({
            "manifest": {"total_row_count": 3},
            "result": {"data_array": [["id"], ["region"]], "next_chunk_index": 1}
        });
        assert!(super::statement_first_column(&chunked).is_err());
        let ok = super::statement_succeeded(ok).unwrap();
        assert_eq!(
            super::statement_first_column(&ok).unwrap(),
            vec!["id", "region"]
        );
        let empty = serde_json::json!({
            "status": {"state": "SUCCEEDED"},
            "manifest": {"total_row_count": 0},
            "result": {}
        });
        assert!(super::statement_first_column(&empty).unwrap().is_empty());
        let truncated = serde_json::json!({"manifest": {"total_row_count": 3}, "result": {}});
        assert!(super::statement_first_column(&truncated).is_err());
        let failed = serde_json::json!({
            "status": {"state": "FAILED", "error": {"message": "PARSE_SYNTAX_ERROR"}}
        });
        let err = super::statement_succeeded(failed).unwrap_err();
        assert!(err.to_string().contains("PARSE_SYNTAX_ERROR"), "{err}");
        let pending = serde_json::json!({"status": {"state": "PENDING"}});
        assert!(super::statement_running(&pending));
        assert!(!super::statement_running(&ok));
        assert!(super::statement_succeeded(pending).is_err());
    }

    #[tokio::test]
    async fn delta_publish_adds_missing_published_columns_and_rejects_retypes() {
        use skippr_runtime_sdk::discover::SkipprDataType;
        let dir = tempfile::tempdir().unwrap();
        let uri = format!("file://{}", dir.path().display());
        let plugin = super::DataSinkDatabricksPlugin::new_with_config(
            "buffer".into(),
            super::DataSinkDatabricksPluginConfig {
                workspace_url: String::new(),
                token: String::new(),
                warehouse_id: None,
                catalog: None,
                schema: None,
                table: None,
                delta_table_uri: Some(uri.clone()),
                storage_options: None,
            },
            Vec::new(),
        )
        .await;
        plugin
            .ensure_delta_table(&uri, &output_with(&[("id", 1, SkipprDataType::Long)]))
            .await
            .unwrap();
        plugin
            .ensure_delta_table(
                &uri,
                &output_with(&[
                    ("id", 1, SkipprDataType::Long),
                    ("Region", 2, SkipprDataType::String),
                ]),
            )
            .await
            .unwrap();
        let table = deltalake::open_table(deltalake::ensure_table_uri(&uri).unwrap())
            .await
            .unwrap();
        let names: Vec<String> = table
            .snapshot()
            .unwrap()
            .schema()
            .fields()
            .map(|field| field.name().to_string())
            .collect();
        assert_eq!(names, vec!["id".to_string(), "Region".to_string()]);
        let err = plugin
            .ensure_delta_table(&uri, &output_with(&[("id", 1, SkipprDataType::String)]))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("does not match published"),
            "{err}"
        );
    }
}
