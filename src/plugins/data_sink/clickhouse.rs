use async_trait::async_trait;
use datafusion::arrow::array::*;
use datafusion::arrow::datatypes::DataType as ArrowDataType;
use datafusion::execution::SendableRecordBatchStream;
use futures::StreamExt;
use reqwest::Client;
use serde_derive::Deserialize;
use tracing::{error, info};

use crate::buffer::BufferChunker;
use crate::helpers::configuration::DataSinkPluginConfig;
use crate::plugins::{DataSink, SchemaSink};

#[derive(Debug, Deserialize, Clone)]
pub struct DataSinkClickhousePluginConfig {
    pub url: String,
    pub database: Option<String>,
    pub user: Option<String>,
    pub password: Option<String>,
    pub table: Option<String>,
    pub format: Option<String>,
}

impl From<DataSinkPluginConfig> for DataSinkClickhousePluginConfig {
    fn from(plugin_config: DataSinkPluginConfig) -> Self {
        match plugin_config {
            DataSinkPluginConfig::Clickhouse(config) => config,
            _ => panic!("Invalid plugin type for ClickHouse"),
        }
    }
}

pub struct DataSinkClickhousePlugin {
    config: DataSinkClickhousePluginConfig,
    #[allow(dead_code)]
    buffer_name: String,
    client: Client,
}

impl DataSinkClickhousePlugin {
    pub async fn new_with_config(
        buffer_name: String,
        config: DataSinkClickhousePluginConfig,
    ) -> Self {
        Self {
            config,
            buffer_name,
            client: Client::new(),
        }
    }

    fn namespace_to_table_name(namespace: &str) -> String {
        namespace.replace('.', "_").to_lowercase()
    }

    fn arrow_type_to_clickhouse(dt: &ArrowDataType) -> &'static str {
        match dt {
            ArrowDataType::Boolean => "Bool",
            ArrowDataType::Int8 => "Int8",
            ArrowDataType::Int16 => "Int16",
            ArrowDataType::Int32 => "Int32",
            ArrowDataType::Int64 => "Int64",
            ArrowDataType::UInt8 => "UInt8",
            ArrowDataType::UInt16 => "UInt16",
            ArrowDataType::UInt32 => "UInt32",
            ArrowDataType::UInt64 => "UInt64",
            ArrowDataType::Float16 | ArrowDataType::Float32 => "Float32",
            ArrowDataType::Float64 => "Float64",
            ArrowDataType::Date32 | ArrowDataType::Date64 => "Date",
            ArrowDataType::Timestamp(_, _) => "DateTime64(6)",
            ArrowDataType::Utf8 | ArrowDataType::LargeUtf8 => "String",
            _ => "String",
        }
    }

    fn row_to_json(batch: &datafusion::arrow::record_batch::RecordBatch, row: usize) -> String {
        let schema = batch.schema();
        let mut map = serde_json::Map::new();
        for (col_idx, field) in schema.fields().iter().enumerate() {
            let array = batch.column(col_idx);
            let val = if array.is_null(row) {
                serde_json::Value::Null
            } else {
                match array.data_type() {
                    ArrowDataType::Boolean => {
                        let a = array.as_any().downcast_ref::<BooleanArray>().unwrap();
                        serde_json::Value::Bool(a.value(row))
                    }
                    ArrowDataType::Int8 => serde_json::json!(array.as_any().downcast_ref::<Int8Array>().unwrap().value(row)),
                    ArrowDataType::Int16 => serde_json::json!(array.as_any().downcast_ref::<Int16Array>().unwrap().value(row)),
                    ArrowDataType::Int32 => serde_json::json!(array.as_any().downcast_ref::<Int32Array>().unwrap().value(row)),
                    ArrowDataType::Int64 => serde_json::json!(array.as_any().downcast_ref::<Int64Array>().unwrap().value(row)),
                    ArrowDataType::UInt8 => serde_json::json!(array.as_any().downcast_ref::<UInt8Array>().unwrap().value(row)),
                    ArrowDataType::UInt16 => serde_json::json!(array.as_any().downcast_ref::<UInt16Array>().unwrap().value(row)),
                    ArrowDataType::UInt32 => serde_json::json!(array.as_any().downcast_ref::<UInt32Array>().unwrap().value(row)),
                    ArrowDataType::UInt64 => serde_json::json!(array.as_any().downcast_ref::<UInt64Array>().unwrap().value(row)),
                    ArrowDataType::Float32 => serde_json::json!(array.as_any().downcast_ref::<Float32Array>().unwrap().value(row)),
                    ArrowDataType::Float64 => serde_json::json!(array.as_any().downcast_ref::<Float64Array>().unwrap().value(row)),
                    ArrowDataType::Utf8 => {
                        let a = array.as_any().downcast_ref::<StringArray>().unwrap();
                        serde_json::Value::String(a.value(row).to_string())
                    }
                    ArrowDataType::LargeUtf8 => {
                        let a = array.as_any().downcast_ref::<LargeStringArray>().unwrap();
                        serde_json::Value::String(a.value(row).to_string())
                    }
                    _ => {
                        if let Some(a) = array.as_any().downcast_ref::<StringArray>() {
                            serde_json::Value::String(a.value(row).to_string())
                        } else {
                            serde_json::Value::Null
                        }
                    }
                }
            };
            map.insert(field.name().clone(), val);
        }
        serde_json::to_string(&map).unwrap_or_else(|_| "{}".to_string())
    }

    async fn execute_ddl(&self, sql: &str) -> Result<(), std::io::Error> {
        let mut req = self.client.post(&self.config.url).body(sql.to_string());
        if let Some(ref user) = self.config.user {
            req = req.header("X-ClickHouse-User", user.as_str());
        }
        if let Some(ref password) = self.config.password {
            req = req.header("X-ClickHouse-Key", password.as_str());
        }
        if let Some(ref db) = self.config.database {
            req = req.header("X-ClickHouse-Database", db.as_str());
        }
        let resp = req
            .send()
            .await
            .map_err(|e| std::io::Error::other(format!("ClickHouse DDL failed: {}", e)))?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(std::io::Error::other(format!(
                "ClickHouse DDL error: {}",
                body
            )));
        }
        Ok(())
    }

    async fn ensure_table(
        &self,
        table_name: &str,
        col_defs: &[(String, &str)],
    ) -> Result<(), std::io::Error> {
        if col_defs.is_empty() {
            return Ok(());
        }
        let cols_sql: Vec<String> = col_defs
            .iter()
            .map(|(name, ch_type)| format!("`{}` Nullable({})", name, ch_type))
            .collect();
        let ddl = format!(
            "CREATE TABLE IF NOT EXISTS `{}` ({}) ENGINE = MergeTree() ORDER BY tuple()",
            table_name,
            cols_sql.join(", ")
        );
        info!("ClickHouse DDL: {}", ddl);
        self.execute_ddl(&ddl).await
    }

    async fn insert_json_rows(
        &self,
        table_name: &str,
        rows: &[String],
    ) -> Result<(), std::io::Error> {
        let body = rows.join("\n");
        let query = format!("INSERT INTO `{}` FORMAT JSONEachRow", table_name);
        let url = format!("{}/?query={}", self.config.url, urlencoding(&query));
        let mut req = self.client.post(&url).body(body);
        if let Some(ref user) = self.config.user {
            req = req.header("X-ClickHouse-User", user.as_str());
        }
        if let Some(ref password) = self.config.password {
            req = req.header("X-ClickHouse-Key", password.as_str());
        }
        if let Some(ref db) = self.config.database {
            req = req.header("X-ClickHouse-Database", db.as_str());
        }
        let resp = req
            .send()
            .await
            .map_err(|e| std::io::Error::other(format!("ClickHouse insert failed: {}", e)))?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(std::io::Error::other(format!(
                "ClickHouse insert error: {}",
                body
            )));
        }
        Ok(())
    }
}

fn urlencoding(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

#[async_trait]
impl DataSink for DataSinkClickhousePlugin {
    async fn sync(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
    ) -> Result<(), std::io::Error> {
        use crate::metrics::counters;
        counters::inc_uploads_in_flight();

        let namespace = BufferChunker::decode_file_namespace(&filename);
        let table_name = self
            .config
            .table
            .clone()
            .unwrap_or_else(|| Self::namespace_to_table_name(&namespace));
        let arrow_schema = stream.schema();

        let col_defs: Vec<(String, &str)> = arrow_schema
            .fields()
            .iter()
            .map(|f| (f.name().clone(), Self::arrow_type_to_clickhouse(f.data_type())))
            .collect();

        self.ensure_table(&table_name, &col_defs).await.map_err(|e| {
            counters::dec_uploads_in_flight();
            e
        })?;

        let mut total_rows = 0usize;
        let mut stream = stream;

        while let Some(batch_result) = stream.next().await {
            let batch = batch_result
                .map_err(|e| std::io::Error::other(format!("stream error: {}", e)))?;
            let num_rows = batch.num_rows();
            if num_rows == 0 {
                continue;
            }

            let json_rows: Vec<String> = (0..num_rows)
                .map(|row| Self::row_to_json(&batch, row))
                .collect();

            match self.insert_json_rows(&table_name, &json_rows).await {
                Ok(_) => {
                    total_rows += num_rows;
                    counters::add_parquet_rows(num_rows as u64);
                    info!(
                        "ClickHouse: inserted {} rows into {} (total: {})",
                        num_rows, table_name, total_rows
                    );
                }
                Err(e) => {
                    error!("ClickHouse INSERT failed: {}", e);
                    counters::dec_uploads_in_flight();
                    return Err(e);
                }
            }
        }

        counters::add_upload(1);
        counters::dec_uploads_in_flight();
        info!(
            "ClickHouse sync complete: {} total rows into {}",
            total_rows, table_name
        );
        Ok(())
    }
}

#[async_trait]
impl SchemaSink for DataSinkClickhousePlugin {
    async fn sync_schema(
        &self,
        namespace: &str,
        metadata: &crate::discover::OutputMetadata,
    ) -> Result<(), std::io::Error> {
        use crate::converters::skippr_arrow::convert_skippr_to_arrow;

        let fields: std::collections::HashMap<String, crate::discover::OutputMetadata> =
            metadata
                .fields
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();

        let arrow_schema = convert_skippr_to_arrow(Box::new(fields)).map_err(|e| {
            std::io::Error::other(format!(
                "Arrow schema conversion for '{}': {}",
                namespace, e
            ))
        })?;

        let table_name = self
            .config
            .table
            .clone()
            .unwrap_or_else(|| Self::namespace_to_table_name(namespace));
        let col_defs: Vec<(String, &str)> = arrow_schema
            .fields()
            .iter()
            .map(|f| {
                (
                    f.name().clone(),
                    Self::arrow_type_to_clickhouse(f.data_type()),
                )
            })
            .collect();

        self.ensure_table(&table_name, &col_defs).await
    }
}
