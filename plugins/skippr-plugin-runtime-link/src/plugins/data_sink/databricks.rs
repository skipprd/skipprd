use std::collections::HashMap;

use super::parquet_util::serialize_to_parquet;
use crate::buffer::BufferChunker;
use crate::helpers::configuration::DataSinkPluginConfig;
use crate::plugins::DataSink;
use async_trait::async_trait;
use dashmap::DashSet;
use datafusion::arrow::array::*;
use datafusion::arrow::datatypes::DataType as ArrowDataType;
use datafusion::execution::SendableRecordBatchStream;
use futures::StreamExt;
use once_cell::sync::Lazy;
use reqwest::Client;
use serde_derive::Deserialize;
use tracing::{error, info};

static CDC_DDL_ENSURED: Lazy<DashSet<String>> = Lazy::new(DashSet::new);

pub struct DatabricksCdcBackend;

impl super::cdc_apply::CdcApplyBackend for DatabricksCdcBackend {
    const ORDER_TOKEN_TYPE: &'static str = "BINARY";

    fn binary_literal(hex: &str) -> String {
        format!("X'{hex}'")
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct DataSinkDatabricksPluginConfig {
    #[serde(default)]
    pub workspace_url: String,
    #[serde(default)]
    pub token: String,
    pub warehouse_id: Option<String>,
    pub catalog: Option<String>,
    pub schema: Option<String>,
    pub table: Option<String>,
    pub format: Option<String>,
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
}

#[async_trait]
impl DataSink for DataSinkDatabricksPlugin {
    async fn sync(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
        cdc_ctx: Option<&crate::plugins::cdc::SyncContext>,
    ) -> Result<(), std::io::Error> {
        if let Some(ctx) = cdc_ctx {
            return self.sync_cdc(stream, filename, ctx).await;
        }
        if self.config.delta_table_uri.is_some() {
            self.sync_delta(stream, filename).await
        } else {
            self.sync_copy(stream, filename).await
        }
    }

    fn capability(&self) -> Option<&'static crate::plugins::cdc::SinkCapability> {
        Some(&crate::plugins::cdc::sink_capabilities::DATABRICKS)
    }
}

impl DataSinkDatabricksPlugin {
    pub async fn new_with_config(
        _buffer_name: String,
        config: DataSinkDatabricksPluginConfig,
    ) -> Self {
        Self {
            client: Client::new(),
            config,
        }
    }

    async fn sync_copy(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
    ) -> Result<(), std::io::Error> {
        use crate::metrics::counters;
        counters::inc_uploads_in_flight();

        let parquet_bytes = serialize_to_parquet(stream).await?;

        let upload_path = format!(
            "/Volumes/{}/{}/staging/{}",
            self.config.catalog.as_deref().unwrap_or("main"),
            self.config.schema.as_deref().unwrap_or("default"),
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
            .map_err(|e| std::io::Error::other(e.to_string()))?
            .error_for_status()
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        if let Some(ref warehouse_id) = self.config.warehouse_id {
            let table = self.config.table.as_deref().unwrap_or("data");
            let sql = format!(
                "COPY INTO {}.{}.{} FROM '{}' FILEFORMAT = PARQUET",
                self.config.catalog.as_deref().unwrap_or("main"),
                self.config.schema.as_deref().unwrap_or("default"),
                table,
                upload_path,
            );

            let stmt_url = format!(
                "{}/api/2.0/sql/statements",
                self.config.workspace_url.trim_end_matches('/'),
            );

            self.client
                .post(&stmt_url)
                .bearer_auth(&self.config.token)
                .json(&serde_json::json!({
                    "warehouse_id": warehouse_id,
                    "statement": sql,
                    "wait_timeout": "30s",
                }))
                .send()
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?
                .error_for_status()
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        }

        info!("Databricks: uploaded {}", upload_path);
        counters::dec_uploads_in_flight();
        Ok(())
    }

    async fn sync_delta(
        &self,
        stream: SendableRecordBatchStream,
        _filename: String,
    ) -> Result<(), std::io::Error> {
        use crate::metrics::counters;
        counters::inc_uploads_in_flight();

        let delta_uri = self.config.delta_table_uri.as_deref().unwrap();
        let storage_opts = self.config.storage_options.clone().unwrap_or_default();

        let parquet_bytes = serialize_to_parquet(stream).await.map_err(|e| {
            counters::dec_uploads_in_flight();
            e
        })?;
        let total_rows = parquet_bytes.meta_data.num_rows as u64;

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

        let table_result =
            deltalake::open_table_with_storage_options(delta_uri, storage_opts.clone()).await;

        match table_result {
            Ok(table) => {
                deltalake::DeltaOps(table)
                    .write(delta_batches)
                    .with_save_mode(deltalake::protocol::SaveMode::Append)
                    .await
                    .map_err(|e| std::io::Error::other(format!("Delta write: {}", e)))?;
            }
            Err(_) => {
                let ops =
                    deltalake::DeltaOps::try_from_uri_with_storage_options(delta_uri, storage_opts)
                        .await
                        .map_err(|e| std::io::Error::other(format!("Delta init: {}", e)))?;

                ops.write(delta_batches)
                    .with_save_mode(deltalake::protocol::SaveMode::Append)
                    .await
                    .map_err(|e| std::io::Error::other(format!("Delta write: {}", e)))?;
            }
        }

        counters::add_parquet_rows(total_rows);
        counters::add_upload(1);
        counters::dec_uploads_in_flight();
        info!("Delta Lake: wrote {} rows to {}", total_rows, delta_uri);
        Ok(())
    }

    async fn execute_sql(&self, sql: &str) -> Result<(), std::io::Error> {
        let warehouse_id = self
            .config
            .warehouse_id
            .as_deref()
            .ok_or_else(|| std::io::Error::other("Databricks CDC requires warehouse_id"))?;
        let url = format!(
            "{}/api/2.0/sql/statements",
            self.config.workspace_url.trim_end_matches('/')
        );
        self.client
            .post(&url)
            .bearer_auth(&self.config.token)
            .json(&serde_json::json!({
                "warehouse_id": warehouse_id,
                "statement": sql,
                "wait_timeout": "60s",
            }))
            .send()
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?
            .error_for_status()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(())
    }

    fn arrow_value_to_sql(array: &dyn Array, row: usize) -> String {
        if array.is_null(row) {
            return "NULL".to_string();
        }
        if let Some(a) = array.as_any().downcast_ref::<StringArray>() {
            return format!("'{}'", a.value(row).replace('\'', "\\'"));
        }
        if let Some(a) = array.as_any().downcast_ref::<LargeStringArray>() {
            return format!("'{}'", a.value(row).replace('\'', "\\'"));
        }
        if let Some(a) = array.as_any().downcast_ref::<Int64Array>() {
            return a.value(row).to_string();
        }
        if let Some(a) = array.as_any().downcast_ref::<Int32Array>() {
            return a.value(row).to_string();
        }
        if let Some(a) = array.as_any().downcast_ref::<Int16Array>() {
            return a.value(row).to_string();
        }
        if let Some(a) = array.as_any().downcast_ref::<Int8Array>() {
            return a.value(row).to_string();
        }
        if let Some(a) = array.as_any().downcast_ref::<UInt64Array>() {
            return a.value(row).to_string();
        }
        if let Some(a) = array.as_any().downcast_ref::<UInt32Array>() {
            return a.value(row).to_string();
        }
        if let Some(a) = array.as_any().downcast_ref::<Float64Array>() {
            return a.value(row).to_string();
        }
        if let Some(a) = array.as_any().downcast_ref::<Float32Array>() {
            return a.value(row).to_string();
        }
        if let Some(a) = array.as_any().downcast_ref::<BooleanArray>() {
            return if a.value(row) { "true" } else { "false" }.to_string();
        }
        format!(
            "'{}'",
            arrow::util::display::array_value_to_string(array, row)
                .unwrap_or_else(|_| "NULL".to_string())
                .replace('\'', "\\'")
        )
    }

    fn arrow_type_to_databricks(dt: &ArrowDataType) -> &'static str {
        match dt {
            ArrowDataType::Boolean => "BOOLEAN",
            ArrowDataType::Int8 | ArrowDataType::Int16 | ArrowDataType::Int32 => "INT",
            ArrowDataType::Int64 => "BIGINT",
            ArrowDataType::Float32 => "FLOAT",
            ArrowDataType::Float64 => "DOUBLE",
            ArrowDataType::Utf8 | ArrowDataType::LargeUtf8 => "STRING",
            ArrowDataType::Binary | ArrowDataType::LargeBinary => "BINARY",
            ArrowDataType::Date32 | ArrowDataType::Date64 => "DATE",
            ArrowDataType::Timestamp(_, _) => "TIMESTAMP",
            _ => "STRING",
        }
    }

    fn namespace_to_table_name(namespace: &str) -> String {
        namespace.replace('.', "_").to_lowercase()
    }

    async fn sync_cdc(
        &self,
        mut stream: SendableRecordBatchStream,
        filename: String,
        ctx: &crate::plugins::cdc::SyncContext,
    ) -> Result<(), std::io::Error> {
        use super::cdc_apply::{
            ddl_add_order_token_column, ddl_create_tombstone_table, delete_if_newer_sql,
            tombstone_table_name, upsert_if_newer_sql,
        };
        use crate::metrics::counters;
        use crate::plugins::cdc::MutationKind;

        let contract = match ctx.contract.as_ref() {
            Some(c) if !c.business_key_columns.is_empty() => c,
            _ => {
                info!(target: "databricks", "CDC context without contract or business keys; falling back to append");
                if self.config.delta_table_uri.is_some() {
                    return self.sync_delta(stream, filename).await;
                } else {
                    return self.sync_copy(stream, filename).await;
                }
            }
        };

        counters::inc_uploads_in_flight();

        let namespace = BufferChunker::decode_file_namespace(&filename);
        let catalog = self.config.catalog.as_deref().unwrap_or("main");
        let schema = self.config.schema.as_deref().unwrap_or("default");
        let table_name_owned = self
            .config
            .table
            .clone()
            .unwrap_or_else(|| Self::namespace_to_table_name(&namespace));
        let table_name = table_name_owned.as_str();
        let arrow_schema = stream.schema();

        let col_defs: Vec<(String, &str)> = arrow_schema
            .fields()
            .iter()
            .map(|f| {
                (
                    f.name().to_lowercase(),
                    Self::arrow_type_to_databricks(f.data_type()),
                )
            })
            .collect();

        let fq_table = format!("\"{}\".\"{}\".\"{}\"", catalog, schema, table_name);

        let create_ddl = format!(
            "CREATE TABLE IF NOT EXISTS {} ({})",
            fq_table,
            col_defs
                .iter()
                .map(|(n, t)| format!("\"{}\" {}", n, t))
                .collect::<Vec<_>>()
                .join(", ")
        );
        self.execute_sql(&create_ddl).await.map_err(|e| {
            counters::dec_uploads_in_flight();
            e
        })?;

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
            let bk_type_pairs: Vec<(String, String)> = contract
                .business_key_columns
                .iter()
                .map(|bk| {
                    let db_type = col_defs
                        .iter()
                        .find(|(name, _)| name == bk)
                        .map(|(_, t)| (*t).to_string())
                        .unwrap_or_else(|| "STRING".to_string());
                    (bk.clone(), db_type)
                })
                .collect();
            let tombstone_ddl =
                ddl_create_tombstone_table::<DatabricksCdcBackend>(&tombstone_tbl, &bk_type_pairs);
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

        let bk_names_quoted: Vec<String> = contract
            .business_key_columns
            .iter()
            .map(|bk| format!("\"{}\"", bk))
            .collect();

        let bk_types: Vec<String> = contract
            .business_key_columns
            .iter()
            .map(|bk| {
                col_defs
                    .iter()
                    .find(|(name, _)| name == bk)
                    .map(|(_, t)| (*t).to_string())
                    .unwrap_or_else(|| "STRING".to_string())
            })
            .collect();

        let col_names_quoted: Vec<String> = arrow_schema
            .fields()
            .iter()
            .map(|f| format!("\"{}\"", f.name().to_lowercase()))
            .collect();

        let mut row_offset = 0usize;
        let mut total_rows = 0usize;

        while let Some(batch_result) = stream.next().await {
            let batch =
                batch_result.map_err(|e| std::io::Error::other(format!("stream error: {}", e)))?;
            let num_rows = batch.num_rows();
            if num_rows == 0 {
                continue;
            }

            for row in 0..num_rows {
                let meta_idx = row_offset + row;
                let row_meta = ctx.part_meta.rows.get(meta_idx).ok_or_else(|| {
                    std::io::Error::other(format!(
                        "CDC row metadata missing at index {} (have {})",
                        meta_idx,
                        ctx.part_meta.rows.len()
                    ))
                })?;

                let order_token_hex: String = row_meta
                    .order_token
                    .iter()
                    .map(|b| format!("{:02x}", b))
                    .collect();

                match row_meta.mutation {
                    MutationKind::Snapshot | MutationKind::Insert | MutationKind::Update => {
                        let mut all_names = col_names_quoted.clone();
                        all_names.push("\"_skippr_order_token\"".to_string());

                        let mut all_values: Vec<String> = (0..batch.num_columns())
                            .map(|col| Self::arrow_value_to_sql(batch.column(col).as_ref(), row))
                            .collect();
                        all_values.push(format!("X'{}'", order_token_hex));

                        let sql = upsert_if_newer_sql::<DatabricksCdcBackend>(
                            &fq_table,
                            &tombstone_table,
                            &all_names,
                            &all_values,
                            &bk_names_quoted,
                            &order_token_hex,
                        );

                        self.execute_sql(&sql).await.map_err(|e| {
                            error!("CDC upsert failed for {}: {}", fq_table, e);
                            counters::dec_uploads_in_flight();
                            e
                        })?;
                    }
                    MutationKind::Delete => {
                        let bk_values: Vec<String> = contract
                            .business_key_columns
                            .iter()
                            .map(|bk| {
                                let col_idx = arrow_schema
                                    .fields()
                                    .iter()
                                    .position(|f| f.name().to_lowercase() == *bk)
                                    .unwrap_or(0);
                                Self::arrow_value_to_sql(batch.column(col_idx).as_ref(), row)
                            })
                            .collect();

                        let sql = delete_if_newer_sql::<DatabricksCdcBackend>(
                            &fq_table,
                            &tombstone_table,
                            &bk_names_quoted,
                            &bk_values,
                            &bk_types,
                            &order_token_hex,
                        );

                        self.execute_sql(&sql).await.map_err(|e| {
                            error!("CDC delete failed for {}: {}", fq_table, e);
                            counters::dec_uploads_in_flight();
                            e
                        })?;
                    }
                }
            }

            row_offset += num_rows;
            total_rows += num_rows;
            counters::add_parquet_rows(num_rows as u64);
            info!(
                "CDC applied {} rows to {}.{}.{} (total: {})",
                num_rows, catalog, schema, table_name, total_rows
            );
        }

        counters::add_upload(1);
        counters::dec_uploads_in_flight();
        info!(
            "Databricks CDC sync complete: {} total rows into {}.{}.{}",
            total_rows, catalog, schema, table_name
        );
        Ok(())
    }
}
