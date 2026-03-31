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
use crate::plugins::DataSink;

#[derive(Debug, Deserialize, Clone)]
pub struct DataSinkMotherduckPluginConfig {
    pub motherduck_token: String,
    pub database: Option<String>,
    pub table: Option<String>,
    pub format: Option<String>,
}

impl From<DataSinkPluginConfig> for DataSinkMotherduckPluginConfig {
    fn from(plugin_config: DataSinkPluginConfig) -> Self {
        match plugin_config {
            DataSinkPluginConfig::Motherduck(config) => config,
            _ => panic!("Invalid plugin type for MotherDuck"),
        }
    }
}

pub struct DataSinkMotherduckPlugin {
    config: DataSinkMotherduckPluginConfig,
    #[allow(dead_code)]
    buffer_name: String,
    client: Client,
}

const MOTHERDUCK_SQL_ENDPOINT: &str = "https://api.motherduck.com/v1/sql";

impl DataSinkMotherduckPlugin {
    pub async fn new_with_config(
        buffer_name: String,
        config: DataSinkMotherduckPluginConfig,
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

    fn arrow_type_to_duckdb(dt: &ArrowDataType) -> &'static str {
        match dt {
            ArrowDataType::Boolean => "BOOLEAN",
            ArrowDataType::Int8 => "TINYINT",
            ArrowDataType::Int16 => "SMALLINT",
            ArrowDataType::Int32 => "INTEGER",
            ArrowDataType::Int64 => "BIGINT",
            ArrowDataType::UInt8 => "UTINYINT",
            ArrowDataType::UInt16 => "USMALLINT",
            ArrowDataType::UInt32 => "UINTEGER",
            ArrowDataType::UInt64 => "UBIGINT",
            ArrowDataType::Float16 | ArrowDataType::Float32 => "FLOAT",
            ArrowDataType::Float64 => "DOUBLE",
            ArrowDataType::Date32 | ArrowDataType::Date64 => "DATE",
            ArrowDataType::Timestamp(_, _) => "TIMESTAMP",
            ArrowDataType::Utf8 | ArrowDataType::LargeUtf8 => "VARCHAR",
            _ => "VARCHAR",
        }
    }

    fn arrow_value_to_sql(array: &dyn Array, row: usize) -> String {
        if array.is_null(row) {
            return "NULL".to_string();
        }
        match array.data_type() {
            ArrowDataType::Boolean => {
                let a = array.as_any().downcast_ref::<BooleanArray>().unwrap();
                if a.value(row) { "TRUE" } else { "FALSE" }.to_string()
            }
            ArrowDataType::Int8 => format!("{}", array.as_any().downcast_ref::<Int8Array>().unwrap().value(row)),
            ArrowDataType::Int16 => format!("{}", array.as_any().downcast_ref::<Int16Array>().unwrap().value(row)),
            ArrowDataType::Int32 => format!("{}", array.as_any().downcast_ref::<Int32Array>().unwrap().value(row)),
            ArrowDataType::Int64 => format!("{}", array.as_any().downcast_ref::<Int64Array>().unwrap().value(row)),
            ArrowDataType::UInt8 => format!("{}", array.as_any().downcast_ref::<UInt8Array>().unwrap().value(row)),
            ArrowDataType::UInt16 => format!("{}", array.as_any().downcast_ref::<UInt16Array>().unwrap().value(row)),
            ArrowDataType::UInt32 => format!("{}", array.as_any().downcast_ref::<UInt32Array>().unwrap().value(row)),
            ArrowDataType::UInt64 => format!("{}", array.as_any().downcast_ref::<UInt64Array>().unwrap().value(row)),
            ArrowDataType::Float32 => format!("{}", array.as_any().downcast_ref::<Float32Array>().unwrap().value(row)),
            ArrowDataType::Float64 => format!("{}", array.as_any().downcast_ref::<Float64Array>().unwrap().value(row)),
            ArrowDataType::Utf8 => {
                let a = array.as_any().downcast_ref::<StringArray>().unwrap();
                format!("'{}'", a.value(row).replace('\'', "''"))
            }
            ArrowDataType::LargeUtf8 => {
                let a = array.as_any().downcast_ref::<LargeStringArray>().unwrap();
                format!("'{}'", a.value(row).replace('\'', "''"))
            }
            _ => {
                if let Some(s) = array.as_any().downcast_ref::<StringArray>() {
                    format!("'{}'", s.value(row).replace('\'', "''"))
                } else {
                    "NULL".to_string()
                }
            }
        }
    }

    async fn execute_sql(&self, sql: &str) -> Result<serde_json::Value, std::io::Error> {
        let mut payload = serde_json::json!({ "sql": sql });
        if let Some(ref db) = self.config.database {
            payload["database"] = serde_json::json!(db);
        }

        let resp = self
            .client
            .post(MOTHERDUCK_SQL_ENDPOINT)
            .header("Authorization", format!("Bearer {}", self.config.motherduck_token))
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await
            .map_err(|e| std::io::Error::other(format!("MotherDuck request: {}", e)))?;

        let status = resp.status();
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| std::io::Error::other(format!("MotherDuck response parse: {}", e)))?;

        if !status.is_success() {
            let msg = body["message"]
                .as_str()
                .or_else(|| body["error"].as_str())
                .unwrap_or("unknown error");
            return Err(std::io::Error::other(format!(
                "MotherDuck API HTTP {}: {}",
                status, msg
            )));
        }

        Ok(body)
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
            .map(|(name, dk_type)| format!("\"{}\" {}", name, dk_type))
            .collect();
        let ddl = format!(
            "CREATE TABLE IF NOT EXISTS \"{}\" ({})",
            table_name,
            cols_sql.join(", ")
        );
        info!("MotherDuck DDL: {}", ddl);
        self.execute_sql(&ddl).await.map(|_| ())
    }
}

#[async_trait]
impl DataSink for DataSinkMotherduckPlugin {
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
            .map(|f| (f.name().to_lowercase(), Self::arrow_type_to_duckdb(f.data_type())))
            .collect();

        let col_names: Vec<String> = arrow_schema
            .fields()
            .iter()
            .map(|f| format!("\"{}\"", f.name().to_lowercase()))
            .collect();
        let col_list = col_names.join(", ");

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

            for chunk_start in (0..num_rows).step_by(1000) {
                let chunk_end = (chunk_start + 1000).min(num_rows);
                let value_rows: Vec<String> = (chunk_start..chunk_end)
                    .map(|row| {
                        let vals: Vec<String> = (0..batch.num_columns())
                            .map(|col| Self::arrow_value_to_sql(batch.column(col).as_ref(), row))
                            .collect();
                        format!("({})", vals.join(", "))
                    })
                    .collect();

                let insert_sql = format!(
                    "INSERT INTO \"{}\" ({}) VALUES {}",
                    table_name, col_list, value_rows.join(", ")
                );

                match self.execute_sql(&insert_sql).await {
                    Ok(_) => {
                        let chunk_rows = chunk_end - chunk_start;
                        total_rows += chunk_rows;
                        counters::add_parquet_rows(chunk_rows as u64);
                    }
                    Err(e) => {
                        error!("MotherDuck INSERT failed: {}", e);
                        counters::dec_uploads_in_flight();
                        return Err(e);
                    }
                }
            }
        }

        counters::add_upload(1);
        counters::dec_uploads_in_flight();
        info!(
            "MotherDuck sync complete: {} total rows into {}",
            total_rows, table_name
        );
        Ok(())
    }
}
