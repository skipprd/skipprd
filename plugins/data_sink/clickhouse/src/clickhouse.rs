use async_trait::async_trait;
use datafusion::execution::SendableRecordBatchStream;
use futures::StreamExt;
use reqwest::Client;
use serde_derive::Deserialize;
use skippr_runtime_sdk::SkipprConfig;
use tracing::{error, info};

use dashmap::DashSet;
use once_cell::sync::Lazy;

use crate::helpers::configuration::DataSinkPluginConfig;
use skippr_runtime_sdk::discover::{OutputMetadata, SkipprDataType};
use skippr_runtime_sdk::plugins::alter_ddl::{
    dest_table_name, missing_published_columns, published_columns_folded,
    require_published_columns_live, DestPublishCache, IdentCase, InstalledDestSchema,
    PublishedBatchColumns,
};
use skippr_runtime_sdk::plugins::DataSink;
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
        business_key_cols: &[(String, &str)],
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
    installed: tokio::sync::RwLock<InstalledDestSchema>,
    publish_cache: DestPublishCache,
}

skippr_runtime_sdk::declare_sink_spec!(
    ClickhouseSinkSpec,
    DataSinkClickhousePlugin,
    skippr_runtime_sdk::plugins::cdc::sink_capabilities::CLICKHOUSE,
    skippr_runtime_sdk::plugins::AtLeastOnceMessageDelivery
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
            installed: tokio::sync::RwLock::new(InstalledDestSchema::default()),
            publish_cache: DestPublishCache::default(),
        }
    }

    fn dest_table(&self, namespace: &str) -> String {
        self.config
            .table
            .clone()
            .unwrap_or_else(|| dest_table_name(namespace))
    }

    fn sql_type(ty: &SkipprDataType) -> Result<&'static str, std::io::Error> {
        Ok(match ty {
            SkipprDataType::Boolean => "Bool",
            SkipprDataType::Byte => "Int8",
            SkipprDataType::Short => "Int16",
            SkipprDataType::Integer => "Int32",
            SkipprDataType::Long => "Int64",
            SkipprDataType::Float => "Float32",
            SkipprDataType::Double => "Float64",
            SkipprDataType::Date => "Date",
            SkipprDataType::Timestamp | SkipprDataType::TimestampMilli => "DateTime64(6)",
            SkipprDataType::String | SkipprDataType::Uuid | SkipprDataType::Json => "String",
            SkipprDataType::Binary | SkipprDataType::Fixed => "String",
            other => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("ClickHouse publish has no SQL type for {other}"),
                ))
            }
        })
    }

    fn quote_ident(name: &str) -> String {
        format!("`{}`", name.replace('`', "``"))
    }

    fn publish_sql(
        table_name: &str,
        metadata: &OutputMetadata,
        existing: &[String],
    ) -> Result<Vec<String>, std::io::Error> {
        let columns = published_columns_folded(metadata, IdentCase::Exact)?;
        let table = Self::quote_ident(table_name);
        if existing.is_empty() {
            let defs = columns
                .iter()
                .map(|column| {
                    Ok(format!(
                        "{} Nullable({})",
                        Self::quote_ident(&column.name),
                        Self::sql_type(&column.ty)?
                    ))
                })
                .collect::<Result<Vec<_>, std::io::Error>>()?;
            return Ok(vec![format!(
                "CREATE TABLE IF NOT EXISTS {} ({}) ENGINE = MergeTree() ORDER BY tuple()",
                table,
                defs.join(", ")
            )]);
        }
        missing_published_columns(&columns, existing, IdentCase::Exact)
            .into_iter()
            .map(|column| {
                Ok(format!(
                    "ALTER TABLE {} ADD COLUMN IF NOT EXISTS {} Nullable({})",
                    table,
                    Self::quote_ident(&column.name),
                    Self::sql_type(&column.ty)?
                ))
            })
            .collect()
    }

    async fn publish_namespace(&self, namespace: &str) -> Result<(), std::io::Error> {
        let (version, metadata) = self.installed.read().await.get(namespace)?;
        self.publish_cache
            .publish(namespace, version, || async {
                let table_name = self.dest_table(namespace);
                let existing = self.table_columns(&table_name).await?;
                for sql in Self::publish_sql(&table_name, &metadata, &existing)? {
                    info!("ClickHouse DDL: {}", sql);
                    self.execute_ddl(&sql).await?;
                }
                require_published_columns_live(
                    &published_columns_folded(&metadata, IdentCase::Exact)?,
                    &self.table_columns(&table_name).await?,
                    IdentCase::Exact,
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
            IdentCase::Exact,
            Self::sql_type,
        )
    }

    async fn sync_append(
        &self,
        mut stream: SendableRecordBatchStream,
        filename: &str,
    ) -> Result<(), std::io::Error> {
        use skippr_runtime_sdk::metrics::counters;
        let namespace = BufferChunker::decode_file_namespace(filename);
        self.batch_columns(&namespace, &stream.schema()).await?;
        let table_name = self.dest_table(&namespace);
        counters::inc_uploads_in_flight();
        let mut total_rows = 0usize;
        while let Some(batch_result) = stream.next().await {
            let batch = batch_result.map_err(|e| {
                counters::dec_uploads_in_flight();
                std::io::Error::other(format!("stream error: {}", e))
            })?;
            let num_rows = batch.num_rows();
            if num_rows == 0 {
                continue;
            }
            let json_rows = (0..num_rows)
                .map(|row| Self::row_to_json(&batch, row))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| {
                    counters::dec_uploads_in_flight();
                    e
                })?;
            if let Err(e) = self.insert_json_rows(&table_name, &json_rows).await {
                error!("ClickHouse INSERT failed: {}", e);
                counters::dec_uploads_in_flight();
                return Err(e);
            }
            total_rows += num_rows;
            counters::add_parquet_rows(num_rows as u64);
            info!(
                "ClickHouse: inserted {} rows into {} (total: {})",
                num_rows, table_name, total_rows
            );
        }
        counters::add_upload(1);
        counters::dec_uploads_in_flight();
        info!(
            "ClickHouse sync complete: {} total rows into {}",
            total_rows, table_name
        );
        Ok(())
    }

    fn row_to_json(
        batch: &datafusion::arrow::record_batch::RecordBatch,
        row: usize,
    ) -> Result<String, std::io::Error> {
        let schema = batch.schema();
        let mut map = serde_json::Map::new();
        for (col_idx, field) in schema.fields().iter().enumerate() {
            map.insert(
                field.name().clone(),
                cdc_apply_value_to_clickhouse_json(&crate::cdc_apply::arrow_value_to_cdc_apply(
                    batch.column(col_idx).as_ref(),
                    row,
                )?)?,
            );
        }
        serde_json::to_string(&map)
            .map_err(|err| std::io::Error::other(format!("ClickHouse JSONEachRow encode: {err}")))
    }

    async fn query_text(&self, sql: &str) -> Result<String, std::io::Error> {
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
            .map_err(|e| std::io::Error::other(format!("ClickHouse query failed: {}", e)))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(std::io::Error::other(format!(
                "ClickHouse query error: {}",
                body
            )));
        }
        Ok(body)
    }

    async fn execute_ddl(&self, sql: &str) -> Result<(), std::io::Error> {
        self.query_text(sql).await.map(|_| ())
    }

    async fn table_columns(&self, table_name: &str) -> Result<Vec<String>, std::io::Error> {
        let db_filter = match &self.config.database {
            Some(db) => format!(" AND database = '{}'", db.replace('\'', "''")),
            None => " AND database = currentDatabase()".to_string(),
        };
        let sql = format!(
            "SELECT name FROM system.columns WHERE table = '{}'{} FORMAT TabSeparated",
            table_name.replace('\'', "''"),
            db_filter
        );
        let body = self.query_text(&sql).await?;
        Ok(body
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect())
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
                return self.sync_append(stream, &filename).await;
            }
        };

        let namespace = BufferChunker::decode_file_namespace(&filename);
        let table_name = self.dest_table(&namespace);
        let col_defs = self.batch_columns(&namespace, &stream.schema()).await?;
        let key_types = col_defs.key_types(&contract.business_key_columns)?;
        let business_key_columns: Vec<String> =
            key_types.iter().map(|(name, _)| name.clone()).collect();
        counters::inc_uploads_in_flight();

        let fq_table = format!("\"{}\"", table_name);

        if CDC_DDL_ENSURED.insert(fq_table.clone()) {
            let order_col_ddl = ddl_add_order_token_column::<ClickhouseCdcBackend>(&fq_table);
            if let Err(e) = self.execute_ddl(&order_col_ddl).await {
                CDC_DDL_ENSURED.remove(&fq_table);
                counters::dec_uploads_in_flight();
                return Err(e);
            }

            let tombstone_tbl = tombstone_table_name(&fq_table);
            let tombstone_ddl =
                ddl_create_tombstone_table::<ClickhouseCdcBackend>(&tombstone_tbl, &key_types);
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
                    let bk_values: Vec<String> = business_key_columns
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
        let namespace = BufferChunker::decode_file_namespace(&filename);
        self.publish_namespace(&namespace).await?;
        match cdc_ctx {
            Some(ctx) => self.sync_cdc(stream, filename, ctx).await,
            None => self.sync_append(stream, &filename).await,
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
        &skippr_runtime_sdk::plugins::cdc::sink_capabilities::CLICKHOUSE
    }

    fn installed_dest_schema(&self) -> Option<&tokio::sync::RwLock<InstalledDestSchema>> {
        Some(&self.installed)
    }
}

fn cdc_apply_value_to_clickhouse_json(
    value: &crate::cdc_apply::CdcApplyValue,
) -> Result<serde_json::Value, std::io::Error> {
    use crate::cdc_apply::CdcApplyValue;
    Ok(match value {
        CdcApplyValue::Null => serde_json::Value::Null,
        CdcApplyValue::Boolean(value) => serde_json::Value::Bool(*value),
        CdcApplyValue::Signed(value) => serde_json::json!(value),
        CdcApplyValue::Unsigned(value) => serde_json::json!(value),
        CdcApplyValue::Float(value) => match value
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
        {
            Some(number) => serde_json::Value::Number(number),
            None => serde_json::Value::String(value.clone()),
        },
        CdcApplyValue::Text(value) | CdcApplyValue::Date(value) => {
            serde_json::Value::String(value.clone())
        }
        CdcApplyValue::Timestamp(value) => {
            serde_json::Value::String(value.replace('T', " ").replace('Z', ""))
        }
        CdcApplyValue::Binary(bytes) => {
            serde_json::Value::String(crate::cdc_apply::encode_hex_bytes(bytes))
        }
    })
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
    fn publish_sql_uses_metadata_and_does_not_retype() {
        use skippr_runtime_sdk::discover::SkipprDataType;
        let metadata = output_with(&[
            ("id", 1, SkipprDataType::Long),
            ("region", 2, SkipprDataType::String),
        ]);
        let created =
            super::DataSinkClickhousePlugin::publish_sql("trips", &metadata, &[]).unwrap();
        assert_eq!(
            created,
            vec![
                "CREATE TABLE IF NOT EXISTS `trips` (`id` Nullable(Int64), `region` Nullable(String)) ENGINE = MergeTree() ORDER BY tuple()"
            ]
        );
        let added =
            super::DataSinkClickhousePlugin::publish_sql("trips", &metadata, &["id".to_string()])
                .unwrap();
        assert_eq!(
            added,
            vec!["ALTER TABLE `trips` ADD COLUMN IF NOT EXISTS `region` Nullable(String)"]
        );
        assert!(added.iter().all(|sql| !sql.contains("CREATE TABLE")));
        let err = super::DataSinkClickhousePlugin::sql_type(&SkipprDataType::Record).unwrap_err();
        assert!(err.to_string().contains("no SQL type"), "{err}");
        assert_eq!(
            super::DataSinkClickhousePlugin::sql_type(&SkipprDataType::Binary).unwrap(),
            "String",
            "ClickHouse stores bytes as String"
        );
        assert_eq!(
            super::DataSinkClickhousePlugin::sql_type(&SkipprDataType::Fixed).unwrap(),
            "String",
            "JSONEachRow carries fixed bytes as hex, like Binary"
        );
        for ty in [
            SkipprDataType::Time,
            SkipprDataType::Decimal,
            SkipprDataType::Unknown,
        ] {
            let err = super::DataSinkClickhousePlugin::sql_type(&ty).unwrap_err();
            assert!(err.to_string().contains("no SQL type"), "{ty:?} {err}");
        }
    }

    #[test]
    fn row_to_json_encodes_published_temporal_and_binary_types() {
        use datafusion::arrow::array::{
            BinaryArray, Date32Array, FixedSizeBinaryArray, TimestampMicrosecondArray,
        };
        use datafusion::arrow::datatypes::{DataType, Field, Schema, TimeUnit};
        use datafusion::arrow::record_batch::RecordBatch;
        use std::sync::Arc;

        let schema = Schema::new(vec![
            Field::new("event_day", DataType::Date32, false),
            Field::new(
                "event_ts",
                DataType::Timestamp(TimeUnit::Microsecond, None),
                false,
            ),
            Field::new("payload", DataType::Binary, false),
            Field::new("token", DataType::FixedSizeBinary(16), false),
        ]);
        let token = [0xab; 16];
        let batch = RecordBatch::try_new(
            Arc::new(schema),
            vec![
                Arc::new(Date32Array::from(vec![0])),
                Arc::new(TimestampMicrosecondArray::from(vec![0])),
                Arc::new(BinaryArray::from_vec(vec![b"hi".as_slice()])),
                Arc::new(
                    FixedSizeBinaryArray::try_from_iter(std::iter::once(token.as_slice())).unwrap(),
                ),
            ],
        )
        .unwrap();
        let json = super::DataSinkClickhousePlugin::row_to_json(&batch, 0).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["event_day"], "1970-01-01");
        let event_ts = value["event_ts"].as_str().expect("timestamp json");
        assert!(event_ts.starts_with("1970-01-01 00:00:00"), "{event_ts}");
        assert!(!event_ts.contains('T'), "{event_ts}");
        assert_eq!(value["payload"], "6869");
        assert_eq!(value["token"], "ab".repeat(16));
        assert!(!json.contains("null"), "{json}");

        let struct_schema = Schema::new(vec![Field::new(
            "nested",
            DataType::Struct(vec![Field::new("k", DataType::Utf8, true)].into()),
            true,
        )]);
        let nested = RecordBatch::try_new(
            Arc::new(struct_schema),
            vec![Arc::new(datafusion::arrow::array::StructArray::from(vec![
                (
                    Arc::new(Field::new("k", DataType::Utf8, true)),
                    Arc::new(datafusion::arrow::array::StringArray::from(vec![Some("a")]))
                        as datafusion::arrow::array::ArrayRef,
                ),
            ]))],
        )
        .unwrap();
        let err = super::DataSinkClickhousePlugin::row_to_json(&nested, 0).unwrap_err();
        assert!(err.to_string().contains("no encoding"), "{err}");
    }

    #[test]
    fn clickhouse_json_writes_floats_as_numbers() {
        use crate::cdc_apply::CdcApplyValue;
        assert_eq!(
            super::cdc_apply_value_to_clickhouse_json(&CdcApplyValue::Float("1.5".into())).unwrap(),
            serde_json::json!(1.5)
        );
        assert_eq!(
            super::cdc_apply_value_to_clickhouse_json(&CdcApplyValue::Float("NaN".into())).unwrap(),
            serde_json::json!("NaN")
        );
    }
}
