use async_trait::async_trait;
use datafusion::arrow::array::*;
use datafusion::arrow::datatypes::DataType as ArrowDataType;
use datafusion::execution::SendableRecordBatchStream;
use futures::StreamExt;
use serde_derive::Deserialize;
use tracing::info;

use crate::buffer::BufferChunker;
use crate::helpers::configuration::DataSinkPluginConfig;
use crate::plugins::DataSink;

#[derive(Debug, Deserialize, Clone)]
pub struct DataSinkDuckdbPluginConfig {
    pub connection_string: String,
    pub motherduck_token: Option<String>,
    pub database: Option<String>,
    pub table: Option<String>,
    pub format: Option<String>,
}

impl From<DataSinkPluginConfig> for DataSinkDuckdbPluginConfig {
    fn from(plugin_config: DataSinkPluginConfig) -> Self {
        match plugin_config {
            DataSinkPluginConfig::Duckdb(config) => config,
            _ => panic!("Invalid plugin type for DuckDB"),
        }
    }
}

pub struct DataSinkDuckdbPlugin {
    config: DataSinkDuckdbPluginConfig,
    #[allow(dead_code)]
    buffer_name: String,
}

impl DataSinkDuckdbPlugin {
    pub async fn new_with_config(
        buffer_name: String,
        config: DataSinkDuckdbPluginConfig,
    ) -> Self {
        Self {
            config,
            buffer_name,
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
}

#[async_trait]
impl DataSink for DataSinkDuckdbPlugin {
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

        let mut all_value_rows: Vec<String> = Vec::new();
        let mut total_rows = 0usize;
        let mut stream = stream;

        while let Some(batch_result) = stream.next().await {
            let batch = batch_result
                .map_err(|e| std::io::Error::other(format!("stream error: {}", e)))?;
            let num_rows = batch.num_rows();
            if num_rows == 0 {
                continue;
            }
            for row in 0..num_rows {
                let vals: Vec<String> = (0..batch.num_columns())
                    .map(|col| Self::arrow_value_to_sql(batch.column(col).as_ref(), row))
                    .collect();
                all_value_rows.push(format!("({})", vals.join(", ")));
            }
            total_rows += num_rows;
        }

        if all_value_rows.is_empty() {
            counters::dec_uploads_in_flight();
            return Ok(());
        }

        let config = self.config.clone();
        let col_defs_owned: Vec<(String, String)> = col_defs
            .into_iter()
            .map(|(n, t)| (n, t.to_string()))
            .collect();
        let col_list_owned = col_list.clone();
        let table_name_owned = table_name.clone();
        let final_total = total_rows;

        tokio::task::spawn_blocking(move || {
            let conn = duckdb::Connection::open(&config.connection_string)
                .map_err(|e| std::io::Error::other(format!("DuckDB open: {}", e)))?;

            if let Some(ref token) = config.motherduck_token {
                conn.execute_batch(&format!("SET motherduck_token='{}'", token))
                    .map_err(|e| std::io::Error::other(format!("DuckDB SET token: {}", e)))?;
            }

            if let Some(ref db) = config.database {
                conn.execute_batch(&format!("USE {}", db))
                    .map_err(|e| std::io::Error::other(format!("DuckDB USE: {}", e)))?;
            }

            let cols_sql: Vec<String> = col_defs_owned
                .iter()
                .map(|(name, dk_type)| format!("\"{}\" {}", name, dk_type))
                .collect();
            let create_ddl = format!(
                "CREATE TABLE IF NOT EXISTS \"{}\" ({})",
                table_name_owned,
                cols_sql.join(", ")
            );
            conn.execute_batch(&create_ddl)
                .map_err(|e| std::io::Error::other(format!("DuckDB DDL: {}", e)))?;

            for chunk in all_value_rows.chunks(1000) {
                let insert_sql = format!(
                    "INSERT INTO \"{}\" ({}) VALUES {}",
                    table_name_owned,
                    col_list_owned,
                    chunk.join(", ")
                );
                conn.execute_batch(&insert_sql)
                    .map_err(|e| std::io::Error::other(format!("DuckDB INSERT: {}", e)))?;
            }

            Ok::<_, std::io::Error>(final_total)
        })
        .await
        .map_err(|e| {
            counters::dec_uploads_in_flight();
            std::io::Error::other(format!("DuckDB spawn: {}", e))
        })??;

        counters::add_parquet_rows(total_rows as u64);
        counters::add_upload(1);
        counters::dec_uploads_in_flight();
        info!(
            "DuckDB sync complete: {} total rows into {}",
            total_rows, table_name
        );
        Ok(())
    }
}
