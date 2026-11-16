use async_trait::async_trait;
use datafusion::arrow::array::*;
use datafusion::arrow::datatypes::DataType as ArrowDataType;
use datafusion::execution::SendableRecordBatchStream;
use futures::StreamExt;
use reqwest::Client;
use serde_derive::Deserialize;
use skippr_runtime_sdk::SkipprConfig;
use tracing::{error, info};

use dashmap::DashSet;
use once_cell::sync::Lazy;

use crate::helpers::configuration::DataSinkPluginConfig;
use skippr_runtime_sdk::plugins::{DataSink, SchemaSink};
use skippr_runtime_sdk::sink_compat::BufferChunker;

static CDC_DDL_ENSURED: Lazy<DashSet<String>> = Lazy::new(DashSet::new);

pub struct ClickhouseCdcBackend;

impl super::cdc_apply::CdcApplyBackend for ClickhouseCdcBackend {
    const ORDER_TOKEN_TYPE: &'static str = "String";

    fn binary_literal(hex: &str) -> String {
        format!("'{hex}'")
    }

    fn ddl_create_tombstone_table(
        fq_tombstone_table: &str,
        business_key_cols: &[(String, String)],
    ) -> String {
        let mut col_defs: Vec<String> = business_key_cols
            .iter()
            .map(|(name, ty)| format!("\"{}\" {} NOT NULL", name, ty))
            .collect();
        col_defs.push("\"_skippr_order_token\" String NOT NULL".to_string());
        col_defs.push("\"_skippr_tie_seq\" Int64 NOT NULL".to_string());
        col_defs.push("\"_skippr_tie_ordinal\" Int32 NOT NULL".to_string());
        col_defs.push("\"_skippr_tie_row\" Int64 NOT NULL".to_string());

        let pk_cols: Vec<String> = business_key_cols
            .iter()
            .map(|(name, _)| format!("\"{}\"", name))
            .collect();

        format!(
            "CREATE TABLE IF NOT EXISTS {} ({}) ENGINE = MergeTree() ORDER BY ({})",
            fq_tombstone_table,
            col_defs.join(", "),
            pk_cols.join(", "),
        )
    }

    fn upsert_if_newer_sql(
        fq_table: &str,
        fq_tombstone_table: &str,
        all_col_names: &[String],
        all_col_values: &[String],
        business_key_names: &[String],
        incoming: &skippr_cdc::StoredWinner,
    ) -> String {
        let token = Self::binary_literal(&hex::encode(&incoming.order));
        let tombstone_bk_match = business_key_names
            .iter()
            .map(|name| {
                let idx = all_col_names.iter().position(|n| n == name).unwrap_or(0);
                format!("{fq_tombstone_table}.{name} = {}", all_col_values[idx])
            })
            .collect::<Vec<_>>()
            .join(" AND ");
        let tombstone_bk_match_unqualified = business_key_names
            .iter()
            .map(|name| {
                let idx = all_col_names.iter().position(|n| n == name).unwrap_or(0);
                format!("{name} = {}", all_col_values[idx])
            })
            .collect::<Vec<_>>()
            .join(" AND ");
        let target_bk_match = business_key_names
            .iter()
            .map(|name| {
                let idx = all_col_names.iter().position(|n| n == name).unwrap_or(0);
                format!("{name} = {}", all_col_values[idx])
            })
            .collect::<Vec<_>>()
            .join(" AND ");
        let update_set = all_col_names
            .iter()
            .zip(all_col_values.iter())
            .filter(|(name, _)| !business_key_names.contains(name))
            .map(|(name, val)| format!("{name} = {val}"))
            .collect::<Vec<_>>()
            .join(", ");
        let incoming_beats = super::cdc_apply::sql_unqualified_loses_to_incoming(&token, incoming);
        let tombstone_beats_incoming =
            super::cdc_apply::sql_stored_beats_incoming(fq_tombstone_table, &token, incoming);

        format!(
            "INSERT INTO {fq_table} ({})\n\
             SELECT {}\n\
             WHERE NOT EXISTS (SELECT 1 FROM {fq_table} WHERE {target_bk_match})\n\
             AND NOT EXISTS (\n\
               SELECT 1 FROM {fq_tombstone_table}\n\
               WHERE {tombstone_bk_match}\n\
               AND {tombstone_beats_incoming}\n\
             );\n\
             ALTER TABLE {fq_table} UPDATE {update_set}\n\
             WHERE {target_bk_match}\n\
             AND {incoming_beats}\n\
             AND NOT EXISTS (\n\
               SELECT 1 FROM {fq_tombstone_table}\n\
               WHERE {tombstone_bk_match}\n\
               AND {tombstone_beats_incoming}\n\
             );\n\
             ALTER TABLE {fq_tombstone_table} DELETE\n\
             WHERE {tombstone_bk_match_unqualified}\n\
             AND {incoming_beats};",
            all_col_names.join(", "),
            all_col_values.join(", "),
        )
    }

    fn delete_if_newer_sql(
        fq_table: &str,
        fq_tombstone_table: &str,
        business_key_names: &[String],
        business_key_values: &[String],
        _business_key_types: &[String],
        incoming: &skippr_cdc::StoredWinner,
    ) -> String {
        let token = Self::binary_literal(&hex::encode(&incoming.order));
        let delete_match = business_key_names
            .iter()
            .zip(business_key_values.iter())
            .map(|(name, value)| format!("{name} = {value}"))
            .collect::<Vec<_>>()
            .join(" AND ");
        let tombstone_bk_match = business_key_names
            .iter()
            .zip(business_key_values.iter())
            .map(|(name, value)| format!("{fq_tombstone_table}.{name} = {value}"))
            .collect::<Vec<_>>();
        let tombstone_cols = business_key_names
            .iter()
            .cloned()
            .chain([
                "\"_skippr_order_token\"".to_string(),
                "\"_skippr_tie_seq\"".to_string(),
                "\"_skippr_tie_ordinal\"".to_string(),
                "\"_skippr_tie_row\"".to_string(),
            ])
            .collect::<Vec<_>>();
        let tombstone_vals = business_key_values
            .iter()
            .cloned()
            .chain([
                token.clone(),
                incoming.tie.seq.to_string(),
                incoming.tie.ordinal.to_string(),
                incoming.tie.row.to_string(),
            ])
            .collect::<Vec<_>>();
        let incoming_beats_row =
            super::cdc_apply::sql_unqualified_loses_to_incoming(&token, incoming);
        let row_beats_incoming = format!(
            "SELECT 1 FROM {fq_table} WHERE {delete_match} AND {}",
            super::cdc_apply::sql_stored_beats_incoming(fq_table, &token, incoming)
        );
        let live_beats_tombstone =
            super::cdc_apply::sql_stored_beats_stored("live", fq_tombstone_table);
        let live_bk = business_key_names
            .iter()
            .zip(business_key_values.iter())
            .map(|(name, value)| format!("live.{name} = {value}"))
            .collect::<Vec<_>>()
            .join(" AND ");
        let live_to_tomb = business_key_names
            .iter()
            .map(|name| format!("live.{name} = tomb.{name}"))
            .collect::<Vec<_>>()
            .join(" AND ");

        format!(
            "ALTER TABLE {fq_table} DELETE\n\
             WHERE {delete_match}\n\
             AND {incoming_beats_row};\n\
             ALTER TABLE {fq_tombstone_table} DELETE\n\
             WHERE {delete_match}\n\
             AND {incoming_beats_row};\n\
             INSERT INTO {fq_tombstone_table} ({})\n\
             SELECT {}\n\
             WHERE NOT EXISTS ({row_beats_incoming})\n\
             AND NOT EXISTS (\n\
               SELECT 1 FROM {fq_tombstone_table}\n\
               WHERE {}\n\
               AND NOT ({incoming_beats_row})\n\
             );\n\
             ALTER TABLE {fq_tombstone_table} DELETE\n\
             WHERE {delete_match}\n\
             AND EXISTS (\n\
               SELECT 1 FROM {fq_table} AS live\n\
               WHERE {live_bk}\n\
               AND {live_beats_tombstone}\n\
             );\n\
             INSERT INTO {fq_tombstone_table} ({})\n\
             SELECT {}, live.\"_skippr_order_token\", live.\"_skippr_tie_seq\", live.\"_skippr_tie_ordinal\", live.\"_skippr_tie_row\"\n\
             FROM {fq_table} AS live\n\
             WHERE {live_bk}\n\
             AND NOT EXISTS (\n\
               SELECT 1 FROM {fq_tombstone_table} AS tomb\n\
               WHERE {live_to_tomb}\n\
             );",
            tombstone_cols.join(", "),
            tombstone_vals.join(", "),
            tombstone_bk_match.join(" AND "),
            tombstone_cols.join(", "),
            business_key_names.join(", "),
        )
    }
}

#[derive(Debug, Deserialize, SkipprConfig, Clone)]
pub struct DataSinkClickhousePluginConfig {
    pub url: String,
    pub database: Option<String>,
    pub user: Option<String>,
    #[skippr(secret)]
    pub password: Option<String>,
    pub table: Option<String>,
}

impl TryFrom<DataSinkPluginConfig> for DataSinkClickhousePluginConfig {
    type Error = String;

    fn try_from(entry: DataSinkPluginConfig) -> Result<Self, Self::Error> {
        entry.decode_for_plugin("Clickhouse")
    }
}

pub struct DataSinkClickhousePlugin {
    config: DataSinkClickhousePluginConfig,
    #[allow(dead_code)]
    buffer_name: String,
    client: Client,
}

skippr_runtime_sdk::declare_sink_spec!(
    ClickhouseSinkSpec,
    DataSinkClickhousePlugin,
    skippr_runtime_sdk::plugins::cdc::sink_capabilities::CLICKHOUSE,
    skippr_runtime_sdk::plugins::AtLeastOnceMessageDelivery
);

skippr_runtime_sdk::declare_schema_sink_spec!(
    ClickhouseSchemaSinkSpec,
    DataSinkClickhousePlugin,
    "Clickhouse"
);

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
                    ArrowDataType::Int8 => serde_json::json!(array
                        .as_any()
                        .downcast_ref::<Int8Array>()
                        .unwrap()
                        .value(row)),
                    ArrowDataType::Int16 => serde_json::json!(array
                        .as_any()
                        .downcast_ref::<Int16Array>()
                        .unwrap()
                        .value(row)),
                    ArrowDataType::Int32 => serde_json::json!(array
                        .as_any()
                        .downcast_ref::<Int32Array>()
                        .unwrap()
                        .value(row)),
                    ArrowDataType::Int64 => serde_json::json!(array
                        .as_any()
                        .downcast_ref::<Int64Array>()
                        .unwrap()
                        .value(row)),
                    ArrowDataType::UInt8 => serde_json::json!(array
                        .as_any()
                        .downcast_ref::<UInt8Array>()
                        .unwrap()
                        .value(row)),
                    ArrowDataType::UInt16 => serde_json::json!(array
                        .as_any()
                        .downcast_ref::<UInt16Array>()
                        .unwrap()
                        .value(row)),
                    ArrowDataType::UInt32 => serde_json::json!(array
                        .as_any()
                        .downcast_ref::<UInt32Array>()
                        .unwrap()
                        .value(row)),
                    ArrowDataType::UInt64 => serde_json::json!(array
                        .as_any()
                        .downcast_ref::<UInt64Array>()
                        .unwrap()
                        .value(row)),
                    ArrowDataType::Float32 => serde_json::json!(array
                        .as_any()
                        .downcast_ref::<Float32Array>()
                        .unwrap()
                        .value(row)),
                    ArrowDataType::Float64 => serde_json::json!(array
                        .as_any()
                        .downcast_ref::<Float64Array>()
                        .unwrap()
                        .value(row)),
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

    async fn execute_sql(&self, sql: &str) -> Result<(), std::io::Error> {
        for stmt in sql.split(";\n") {
            let stmt = stmt.trim().trim_end_matches(';').trim();
            if stmt.is_empty() {
                continue;
            }
            self.execute_ddl(stmt).await?;
        }
        Ok(())
    }

    async fn sync_cdc(
        &self,
        mut stream: SendableRecordBatchStream,
        filename: String,
        ctx: &skippr_runtime_sdk::plugins::cdc::SyncContext,
    ) -> Result<(), std::io::Error> {
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
                info!(target: "clickhouse", "CDC context without contract or business keys; falling back to append");
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
                            f.name().clone(),
                            Self::arrow_type_to_clickhouse(f.data_type()),
                        )
                    })
                    .collect();
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
                    let json_rows: Vec<String> = (0..num_rows)
                        .map(|row| Self::row_to_json(&batch, row))
                        .collect();
                    self.insert_json_rows(&table_name, &json_rows)
                        .await
                        .map_err(|e| {
                            counters::dec_uploads_in_flight();
                            e
                        })?;
                    total_rows += num_rows;
                    counters::add_parquet_rows(num_rows as u64);
                }
                counters::add_upload(1);
                counters::dec_uploads_in_flight();
                info!(
                    "ClickHouse CDC fallback append: {} rows into {}",
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
                    f.name().clone(),
                    Self::arrow_type_to_clickhouse(f.data_type()),
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
            let order_col_ddl = ddl_add_order_token_column::<ClickhouseCdcBackend>(&fq_table);
            if let Err(e) = self.execute_ddl(&order_col_ddl).await {
                let msg = e.to_string();
                if !msg.contains("already exists") && !msg.contains("duplicate column") {
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
                    let ch_type = col_defs
                        .iter()
                        .find(|(name, _)| name == bk)
                        .map(|(_, t)| (*t).to_string())
                        .unwrap_or_else(|| "String".to_string());
                    (bk.clone(), ch_type)
                })
                .collect();
            let tombstone_ddl =
                ddl_create_tombstone_table::<ClickhouseCdcBackend>(&tombstone_tbl, &bk_type_pairs);
            if let Err(e) = self.execute_ddl(&tombstone_ddl).await {
                let msg = e.to_string();
                if !msg.contains("already exists") {
                    CDC_DDL_ENSURED.remove(&fq_table);
                    counters::dec_uploads_in_flight();
                    return Err(e);
                }
            }

            info!(target: "clickhouse", "CDC DDL applied for {}", fq_table);
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
                    .unwrap_or_else(|| "String".to_string())
            })
            .collect();

        let col_names_quoted: Vec<String> = arrow_schema
            .fields()
            .iter()
            .map(|f| format!("\"{}\"", f.name()))
            .collect();

        let mut apply_batch = CdcApplyBatch {
            columns: arrow_schema
                .fields()
                .iter()
                .zip(col_defs.iter())
                .map(|(field, (_, target_type))| CdcApplyColumn {
                    name: field.name().clone(),
                    target_type: (*target_type).to_string(),
                })
                .collect(),
            business_key_columns: contract.business_key_columns.clone(),
            rows: Vec::new(),
        };
        let mut row_offset = 0usize;
        while let Some(batch_result) = stream.next().await {
            let batch =
                batch_result.map_err(|e| std::io::Error::other(format!("stream error: {}", e)))?;
            let num_rows = batch.num_rows();
            if num_rows == 0 {
                continue;
            }
            append_record_batch_to_cdc_apply(
                &mut apply_batch,
                &batch,
                &ctx.part_meta.rows,
                &ctx.decoded_ties().map_err(std::io::Error::other)?,
                row_offset,
            )
            .map_err(|error| std::io::Error::other(error.to_string()))?;
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
                        .map(format_cdc_apply_value::<ClickhouseCdcBackend>)
                        .collect();
                    all_values.push(format!("'{}'", hex::encode(&incoming.order)));
                    all_values.push(incoming.tie.seq.to_string());
                    all_values.push(incoming.tie.ordinal.to_string());
                    all_values.push(incoming.tie.row.to_string());
                    let sql = upsert_if_newer_sql::<ClickhouseCdcBackend>(
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
                    let bk_values: Vec<String> = contract
                        .business_key_columns
                        .iter()
                        .map(|bk| {
                            let idx = apply_batch
                                .columns
                                .iter()
                                .position(|column| column.name == *bk)
                                .unwrap_or(0);
                            format_cdc_apply_value::<ClickhouseCdcBackend>(&row.values[idx])
                        })
                        .collect();
                    let sql = delete_if_newer_sql::<ClickhouseCdcBackend>(
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
            info!(
                "CDC applied {} collapsed rows to {}",
                total_rows, table_name
            );
        }

        counters::add_upload(1);
        counters::dec_uploads_in_flight();
        info!(
            "ClickHouse CDC sync complete: {} total rows into {}",
            total_rows, table_name
        );
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
                    f.name().clone(),
                    Self::arrow_type_to_clickhouse(f.data_type()),
                )
            })
            .collect();

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
        &skippr_runtime_sdk::plugins::cdc::sink_capabilities::CLICKHOUSE
    }
}

#[async_trait]
impl SchemaSink for DataSinkClickhousePlugin {
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
                    f.name().clone(),
                    Self::arrow_type_to_clickhouse(f.data_type()),
                )
            })
            .collect();

        self.ensure_table(&table_name, &col_defs).await
    }
}

#[cfg(test)]
mod tests {
    use super::ClickhouseCdcBackend;
    use crate::cdc_apply::delete_if_newer_sql;

    #[test]
    fn delete_sql_syncs_live_winner_onto_tombstone() {
        let incoming = skippr_cdc::StoredWinner {
            order: vec![7],
            tie: skippr_cdc::Tie {
                seq: 1,
                ordinal: 0,
                row: 0,
            },
        };
        let sql = delete_if_newer_sql::<ClickhouseCdcBackend>(
            "\"users\"",
            "\"_skippr_tombstones_users\"",
            &["\"id\"".to_string()],
            &["1".to_string()],
            &["Int64".to_string()],
            &incoming,
        );
        assert!(sql.contains("ALTER TABLE \"users\" DELETE"));
        assert!(sql.contains("ALTER TABLE \"_skippr_tombstones_users\" DELETE"));
        assert!(
            sql.contains("FROM \"users\" AS live"),
            "10/4/7 copies the remaining live winner onto the tombstone"
        );
        assert!(
            sql.contains("NOT EXISTS"),
            "tombstone insert is gated on a live row that beats incoming"
        );
        assert!(
            !sql.contains("FROM \"users\"\n             WHERE \"id\" = 1;"),
            "unguarded INSERT FROM live is illegal"
        );
    }
}
