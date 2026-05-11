use async_trait::async_trait;
use datafusion::arrow::array::*;
use datafusion::arrow::datatypes::DataType as ArrowDataType;
use datafusion::execution::SendableRecordBatchStream;
use futures::StreamExt;
use reqwest::Client;
use serde_derive::Deserialize;
use tracing::{error, info};

use dashmap::DashSet;
use once_cell::sync::Lazy;

use crate::helpers::configuration::DataSinkPluginConfig;
use skippr_runtime_sdk::plugins::{DataSink, SchemaSink};
use skippr_runtime_sdk::sink_compat::BufferChunker;

static CDC_DDL_ENSURED: Lazy<DashSet<String>> = Lazy::new(DashSet::new);

pub struct MotherduckCdcBackend;

impl super::cdc_apply::CdcApplyBackend for MotherduckCdcBackend {
    const ORDER_TOKEN_TYPE: &'static str = "BLOB";

    fn binary_literal(hex: &str) -> String {
        format!("'\\x{hex}'::BLOB")
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct DataSinkMotherduckPluginConfig {
    pub motherduck_token: String,
    pub database: Option<String>,
    pub table: Option<String>,
    pub format: Option<String>,
}

impl TryFrom<DataSinkPluginConfig> for DataSinkMotherduckPluginConfig {
    type Error = String;

    fn try_from(entry: DataSinkPluginConfig) -> Result<Self, Self::Error> {
        entry.decode_for_plugin("Motherduck")
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
            ArrowDataType::Int8 => format!(
                "{}",
                array
                    .as_any()
                    .downcast_ref::<Int8Array>()
                    .unwrap()
                    .value(row)
            ),
            ArrowDataType::Int16 => format!(
                "{}",
                array
                    .as_any()
                    .downcast_ref::<Int16Array>()
                    .unwrap()
                    .value(row)
            ),
            ArrowDataType::Int32 => format!(
                "{}",
                array
                    .as_any()
                    .downcast_ref::<Int32Array>()
                    .unwrap()
                    .value(row)
            ),
            ArrowDataType::Int64 => format!(
                "{}",
                array
                    .as_any()
                    .downcast_ref::<Int64Array>()
                    .unwrap()
                    .value(row)
            ),
            ArrowDataType::UInt8 => format!(
                "{}",
                array
                    .as_any()
                    .downcast_ref::<UInt8Array>()
                    .unwrap()
                    .value(row)
            ),
            ArrowDataType::UInt16 => format!(
                "{}",
                array
                    .as_any()
                    .downcast_ref::<UInt16Array>()
                    .unwrap()
                    .value(row)
            ),
            ArrowDataType::UInt32 => format!(
                "{}",
                array
                    .as_any()
                    .downcast_ref::<UInt32Array>()
                    .unwrap()
                    .value(row)
            ),
            ArrowDataType::UInt64 => format!(
                "{}",
                array
                    .as_any()
                    .downcast_ref::<UInt64Array>()
                    .unwrap()
                    .value(row)
            ),
            ArrowDataType::Float32 => format!(
                "{}",
                array
                    .as_any()
                    .downcast_ref::<Float32Array>()
                    .unwrap()
                    .value(row)
            ),
            ArrowDataType::Float64 => format!(
                "{}",
                array
                    .as_any()
                    .downcast_ref::<Float64Array>()
                    .unwrap()
                    .value(row)
            ),
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
            .header(
                "Authorization",
                format!("Bearer {}", self.config.motherduck_token),
            )
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

    async fn sync_cdc(
        &self,
        mut stream: SendableRecordBatchStream,
        filename: String,
        ctx: &skippr_runtime_sdk::plugins::cdc::SyncContext,
    ) -> Result<(), std::io::Error> {
        use super::cdc_apply::{
            ddl_add_order_token_column, ddl_create_tombstone_table, delete_if_newer_sql,
            tombstone_table_name, upsert_if_newer_sql,
        };
        use skippr_runtime_sdk::metrics::counters;
        use skippr_runtime_sdk::plugins::cdc::MutationKind;

        let contract = match ctx.contract.as_ref() {
            Some(c) if !c.business_key_columns.is_empty() => c,
            _ => {
                info!(target: "motherduck", "CDC context without contract or business keys; falling back to append");
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
                    .map(|f| {
                        (
                            f.name().to_lowercase(),
                            Self::arrow_type_to_duckdb(f.data_type()),
                        )
                    })
                    .collect();
                let col_names: Vec<String> = arrow_schema
                    .fields()
                    .iter()
                    .map(|f| format!("\"{}\"", f.name().to_lowercase()))
                    .collect();
                let col_list = col_names.join(", ");
                self.ensure_table(&table_name, &col_defs)
                    .await
                    .map_err(|e| {
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
                    let value_rows: Vec<String> = (0..num_rows)
                        .map(|row| {
                            let vals: Vec<String> = (0..batch.num_columns())
                                .map(|col| {
                                    Self::arrow_value_to_sql(batch.column(col).as_ref(), row)
                                })
                                .collect();
                            format!("({})", vals.join(", "))
                        })
                        .collect();
                    let insert_sql = format!(
                        "INSERT INTO \"{}\" ({}) VALUES {}",
                        table_name,
                        col_list,
                        value_rows.join(", ")
                    );
                    self.execute_sql(&insert_sql).await.map_err(|e| {
                        counters::dec_uploads_in_flight();
                        e
                    })?;
                    total_rows += num_rows;
                    counters::add_parquet_rows(num_rows as u64);
                }
                counters::add_upload(1);
                counters::dec_uploads_in_flight();
                info!(
                    "MotherDuck CDC fallback append: {} rows into {}",
                    total_rows, table_name
                );
                return Ok(());
            }
        };

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
            .map(|f| {
                (
                    f.name().to_lowercase(),
                    Self::arrow_type_to_duckdb(f.data_type()),
                )
            })
            .collect();

        let fq_table = format!("\"{}\"", table_name);

        self.ensure_table(&table_name, &col_defs)
            .await
            .map_err(|e| {
                counters::dec_uploads_in_flight();
                e
            })?;

        if CDC_DDL_ENSURED.insert(fq_table.clone()) {
            let order_col_ddl = ddl_add_order_token_column::<MotherduckCdcBackend>(&fq_table);
            self.execute_sql(&order_col_ddl).await.map_err(|e| {
                CDC_DDL_ENSURED.remove(&fq_table);
                counters::dec_uploads_in_flight();
                e
            })?;

            let tombstone_tbl = tombstone_table_name(&fq_table);
            let bk_type_pairs: Vec<(String, String)> = contract
                .business_key_columns
                .iter()
                .map(|bk| {
                    let dk_type = col_defs
                        .iter()
                        .find(|(name, _)| name == bk)
                        .map(|(_, t)| (*t).to_string())
                        .unwrap_or_else(|| "VARCHAR".to_string());
                    (bk.clone(), dk_type)
                })
                .collect();
            let tombstone_ddl =
                ddl_create_tombstone_table::<MotherduckCdcBackend>(&tombstone_tbl, &bk_type_pairs);
            self.execute_sql(&tombstone_ddl).await.map_err(|e| {
                CDC_DDL_ENSURED.remove(&fq_table);
                counters::dec_uploads_in_flight();
                e
            })?;

            info!(target: "motherduck", "CDC DDL applied for {}", fq_table);
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
                    .unwrap_or_else(|| "VARCHAR".to_string())
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
                        all_values.push(format!("'\\x{}'::BLOB", order_token_hex));

                        let sql = upsert_if_newer_sql::<MotherduckCdcBackend>(
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

                        let sql = delete_if_newer_sql::<MotherduckCdcBackend>(
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
                "CDC applied {} rows to {} (total: {})",
                num_rows, table_name, total_rows
            );
        }

        counters::add_upload(1);
        counters::dec_uploads_in_flight();
        info!(
            "MotherDuck CDC sync complete: {} total rows into {}",
            total_rows, table_name
        );
        Ok(())
    }
}

#[async_trait]
impl DataSink for DataSinkMotherduckPlugin {
    async fn sync(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
        cdc_ctx: Option<&skippr_runtime_sdk::plugins::cdc::SyncContext>,
    ) -> Result<(), std::io::Error> {
        if let Some(ctx) = cdc_ctx {
            return self.sync_cdc(stream, filename, ctx).await;
        }
        use skippr_runtime_sdk::metrics::counters;
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
            .map(|f| {
                (
                    f.name().to_lowercase(),
                    Self::arrow_type_to_duckdb(f.data_type()),
                )
            })
            .collect();

        let col_names: Vec<String> = arrow_schema
            .fields()
            .iter()
            .map(|f| format!("\"{}\"", f.name().to_lowercase()))
            .collect();
        let col_list = col_names.join(", ");

        self.ensure_table(&table_name, &col_defs)
            .await
            .map_err(|e| {
                counters::dec_uploads_in_flight();
                e
            })?;

        let mut total_rows = 0usize;
        let mut stream = stream;

        while let Some(batch_result) = stream.next().await {
            let batch =
                batch_result.map_err(|e| std::io::Error::other(format!("stream error: {}", e)))?;
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
                    table_name,
                    col_list,
                    value_rows.join(", ")
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

    fn capability(&self) -> Option<&'static skippr_runtime_sdk::plugins::cdc::SinkCapability> {
        Some(&skippr_runtime_sdk::plugins::cdc::sink_capabilities::MOTHERDUCK)
    }
}

#[async_trait]
impl SchemaSink for DataSinkMotherduckPlugin {
    async fn sync_schema(
        &self,
        namespace: &str,
        metadata: &skippr_runtime_sdk::discover::OutputMetadata,
    ) -> Result<(), std::io::Error> {
        use skippr_runtime_sdk::converters::skippr_arrow::convert_skippr_to_arrow;

        let fields: std::collections::HashMap<
            String,
            skippr_runtime_sdk::discover::OutputMetadata,
        > = metadata
            .child_fields()
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
                    f.name().to_lowercase(),
                    Self::arrow_type_to_duckdb(f.data_type()),
                )
            })
            .collect();

        self.ensure_table(&table_name, &col_defs).await
    }
}
