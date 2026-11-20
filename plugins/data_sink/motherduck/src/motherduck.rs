use async_trait::async_trait;
use datafusion::execution::SendableRecordBatchStream;
use futures::StreamExt;
use reqwest::Client;
use serde_derive::Deserialize;
use skippr_runtime_sdk::SkipprConfig;
use tracing::{error, info, warn};

use dashmap::DashSet;
use once_cell::sync::Lazy;

use crate::helpers::configuration::DataSinkPluginConfig;
use skippr_runtime_sdk::plugins::alter_ddl::{
    dest_table_name, publish_warehouse_table, DestPublishCache, InstalledDestSchema,
    PublishedBatchColumns, WarehouseDialect,
};
use skippr_runtime_sdk::plugins::DataSink;
use skippr_runtime_sdk::sink_compat::BufferChunker;

static CDC_DDL_ENSURED: Lazy<DashSet<String>> = Lazy::new(DashSet::new);
const MOTHERDUCK_NATIVE_STAGE_BLOCKER: &str =
    "the HTTP SQL connector exposes no DuckDB appender or documented request-size contract";

pub struct MotherduckCdcBackend;

impl super::cdc_apply::CdcApplyBackend for MotherduckCdcBackend {
    const ORDER_TOKEN_TYPE: &'static str = "BLOB";

    fn binary_literal(hex: &str) -> String {
        format!("'\\x{hex}'::BLOB")
    }
}

#[derive(Debug, Deserialize, SkipprConfig, Clone)]
pub struct DataSinkMotherduckPluginConfig {
    #[skippr(secret)]
    pub motherduck_token: String,
    pub database: Option<String>,
    pub table: Option<String>,
    /// Query/model only; ignored at ingest.
    #[serde(default)]
    #[allow(dead_code)]
    pub schema: Option<String>,
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
    installed: tokio::sync::RwLock<InstalledDestSchema>,
    publish_cache: DestPublishCache,
}

skippr_runtime_sdk::declare_sink_spec!(
    MotherduckSinkSpec,
    DataSinkMotherduckPlugin,
    skippr_runtime_sdk::plugins::cdc::sink_capabilities::MOTHERDUCK,
    skippr_runtime_sdk::plugins::FinalStateIdempotentApply,
    skippr_runtime_sdk::plugins::AlterSupport::Warehouse(
        skippr_runtime_sdk::plugins::alter_ddl::WarehouseDialect::MotherDuck
    )
);

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
            installed: tokio::sync::RwLock::new(InstalledDestSchema::default()),
            publish_cache: DestPublishCache::default(),
        }
    }

    fn table_name(&self, namespace: &str) -> String {
        self.config
            .table
            .clone()
            .unwrap_or_else(|| dest_table_name(namespace))
    }

    fn fq_table(&self, namespace: &str) -> String {
        format!("\"{}\"", self.table_name(namespace))
    }

    async fn publish_namespace(&self, namespace: &str) -> Result<(), std::io::Error> {
        let (version, metadata) = self.installed.read().await.get(namespace)?;
        publish_warehouse_table(
            &self.publish_cache,
            self,
            WarehouseDialect::MotherDuck,
            &self.fq_table(namespace),
            namespace,
            version,
            &metadata,
        )
        .await
    }

    async fn batch_columns(
        &self,
        namespace: &str,
        batch: &datafusion::arrow::datatypes::Schema,
    ) -> Result<PublishedBatchColumns, std::io::Error> {
        self.installed.read().await.warehouse_batch_columns(
            WarehouseDialect::MotherDuck,
            namespace,
            batch,
        )
    }

    async fn sync_append(
        &self,
        mut stream: SendableRecordBatchStream,
        filename: &str,
    ) -> Result<(), std::io::Error> {
        use skippr_runtime_sdk::metrics::counters;
        let namespace = BufferChunker::decode_file_namespace(filename);
        let col_defs = self.batch_columns(&namespace, &stream.schema()).await?;
        let fq_table = self.fq_table(&namespace);
        let col_list = col_defs
            .columns()
            .iter()
            .map(|(name, _)| WarehouseDialect::MotherDuck.quote_ident(name))
            .collect::<Vec<_>>()
            .join(", ");
        counters::inc_uploads_in_flight();
        let mut total_rows = 0usize;
        while let Some(batch_result) = stream.next().await {
            let batch = batch_result.map_err(|e| {
                counters::dec_uploads_in_flight();
                std::io::Error::other(format!("stream error: {}", e))
            })?;
            let num_rows = batch.num_rows();
            for chunk_start in (0..num_rows).step_by(1000) {
                let chunk_end = (chunk_start + 1000).min(num_rows);
                let value_rows = (chunk_start..chunk_end)
                    .map(|row| {
                        crate::cdc_apply::sql_values_row::<MotherduckCdcBackend>(&batch, row)
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| {
                        counters::dec_uploads_in_flight();
                        std::io::Error::from(e)
                    })?;
                let insert_sql = format!(
                    "INSERT INTO {} ({}) VALUES {}",
                    fq_table,
                    col_list,
                    value_rows.join(", ")
                );
                if let Err(e) = self.execute_sql(&insert_sql).await {
                    error!("MotherDuck INSERT failed: {}", e);
                    counters::dec_uploads_in_flight();
                    return Err(e);
                }
                let chunk_rows = chunk_end - chunk_start;
                total_rows += chunk_rows;
                counters::add_parquet_rows(chunk_rows as u64);
            }
        }
        counters::add_upload(1);
        counters::dec_uploads_in_flight();
        info!(
            "MotherDuck sync complete: {} total rows into {}",
            total_rows, fq_table
        );
        Ok(())
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

    async fn sync_cdc(
        &self,
        mut stream: SendableRecordBatchStream,
        filename: String,
        ctx: &skippr_runtime_sdk::plugins::cdc::SyncContext,
    ) -> Result<(), std::io::Error> {
        use super::cdc_apply::{
            append_record_batch_to_cdc_apply, collapse_apply_batch, ddl_add_order_token_column,
            ddl_create_tombstone_table, guarded_warehouse_cdc_row_sql, tombstone_table_name,
            warehouse_bulk_cdc_sql, CdcApplyBatch, CdcApplyColumn, CdcWarehouseDialect,
        };
        use skippr_runtime_sdk::metrics::counters;

        tracing::debug!(
            target: "motherduck",
            blocker = MOTHERDUCK_NATIVE_STAGE_BLOCKER,
            "using bounded SQL staging with guarded overflow"
        );
        let contract = match ctx.contract.as_ref() {
            Some(c) if !c.business_key_columns.is_empty() => c,
            _ => {
                info!(target: "motherduck", "CDC context without contract or business keys; falling back to append");
                return self.sync_append(stream, &filename).await;
            }
        };

        let namespace = BufferChunker::decode_file_namespace(&filename);
        let table_name = self.table_name(&namespace);
        let fq_table = self.fq_table(&namespace);
        let col_defs = self.batch_columns(&namespace, &stream.schema()).await?;
        let key_types = col_defs.key_types(&contract.business_key_columns)?;
        counters::inc_uploads_in_flight();

        if CDC_DDL_ENSURED.insert(fq_table.clone()) {
            let order_col_ddl = ddl_add_order_token_column::<MotherduckCdcBackend>(&fq_table);
            self.execute_sql(&order_col_ddl).await.map_err(|e| {
                CDC_DDL_ENSURED.remove(&fq_table);
                counters::dec_uploads_in_flight();
                e
            })?;

            let tombstone_tbl = tombstone_table_name(&fq_table);
            let tombstone_ddl =
                ddl_create_tombstone_table::<MotherduckCdcBackend>(&tombstone_tbl, &key_types);
            self.execute_sql(&tombstone_ddl).await.map_err(|e| {
                CDC_DDL_ENSURED.remove(&fq_table);
                counters::dec_uploads_in_flight();
                e
            })?;

            info!(target: "motherduck", "CDC DDL applied for {}", fq_table);
        }

        let tombstone_table = tombstone_table_name(&fq_table);

        let mut row_offset = 0usize;
        let mut apply_batch = CdcApplyBatch {
            columns: col_defs
                .columns()
                .iter()
                .map(|(name, target_type)| CdcApplyColumn {
                    name: name.clone(),
                    target_type: (*target_type).to_string(),
                })
                .collect(),
            business_key_columns: key_types.iter().map(|(name, _)| name.clone()).collect(),
            rows: Vec::new(),
        };

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

        let total_rows = apply_batch.rows.len();
        if total_rows > 0 {
            match warehouse_bulk_cdc_sql(
                CdcWarehouseDialect::MotherDuck,
                &fq_table,
                &tombstone_table,
                &apply_batch,
            ) {
                Ok(sql) => {
                    self.execute_sql(&sql.transactional_script(CdcWarehouseDialect::MotherDuck))
                        .await
                        .map_err(|error| {
                            error!(
                                "MotherDuck bulk CDC apply failed for {}: {}",
                                fq_table, error
                            );
                            counters::dec_uploads_in_flight();
                            error
                        })?;
                }
                Err(error) if error.is_warehouse_stage_limit() => {
                    warn!(
                        target: "motherduck",
                        "CDC chunk exceeds defensive HTTP staging envelope; using guarded row apply: {}",
                        error
                    );
                    let collapsed = collapse_apply_batch(&apply_batch);
                    for row in &collapsed.rows {
                        let statement = guarded_warehouse_cdc_row_sql::<MotherduckCdcBackend>(
                            CdcWarehouseDialect::MotherDuck,
                            &fq_table,
                            &tombstone_table,
                            &collapsed,
                            row,
                        )?;
                        self.execute_sql(&statement).await.map_err(|error| {
                            counters::dec_uploads_in_flight();
                            error
                        })?;
                    }
                }
                Err(error) => {
                    counters::dec_uploads_in_flight();
                    return Err(std::io::Error::other(error.to_string()));
                }
            }
            counters::add_parquet_rows(total_rows as u64);
            info!(
                target: "motherduck",
                "CDC safely applied {} rows to {}",
                total_rows,
                table_name
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
        &skippr_runtime_sdk::plugins::cdc::sink_capabilities::MOTHERDUCK
    }

    fn installed_dest_schema(&self) -> Option<&tokio::sync::RwLock<InstalledDestSchema>> {
        Some(&self.installed)
    }

    fn alter_support(&self) -> skippr_runtime_sdk::plugins::AlterSupport {
        skippr_runtime_sdk::plugins::AlterSupport::Warehouse(
            skippr_runtime_sdk::plugins::alter_ddl::WarehouseDialect::MotherDuck,
        )
    }

    async fn apply_alter(
        &self,
        namespace: &str,
        _metadata: &skippr_runtime_sdk::discover::OutputMetadata,
        alter: &skippr_runtime_sdk::discover::schema_alter::CommittedAlter,
    ) -> Result<(), std::io::Error> {
        skippr_runtime_sdk::plugins::alter_ddl::apply_warehouse_alter(
            self,
            WarehouseDialect::MotherDuck,
            &self.fq_table(namespace),
            namespace,
            alter,
        )
        .await
    }
}

#[async_trait]
impl skippr_runtime_sdk::plugins::alter_ddl::WarehouseExec for DataSinkMotherduckPlugin {
    async fn columns(&self, namespace: &str) -> std::io::Result<Vec<String>> {
        let table_name = self.table_name(namespace);
        let sql = format!(
            "SELECT column_name FROM information_schema.columns WHERE table_schema = current_schema() AND table_name = '{}'",
            table_name.replace('\'', "''"),
        );
        let body = self.execute_sql(&sql).await?;
        skippr_runtime_sdk::plugins::alter_ddl::json_result_names(&body)
    }

    async fn execute(&self, sql: &str) -> std::io::Result<()> {
        self.execute_sql(sql).await.map(|_| ())
    }

    async fn query_first(&self, sql: &str) -> std::io::Result<Option<String>> {
        let body = self.execute_sql(sql).await?;
        skippr_runtime_sdk::plugins::alter_ddl::json_result_first(&body)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn motherduck_bulk_cdc_sql_is_one_duckdb_transaction() {
        let batch = crate::cdc_apply::warehouse_sql_test_batch();
        let sql = crate::cdc_apply::warehouse_bulk_cdc_sql(
            crate::cdc_apply::CdcWarehouseDialect::MotherDuck,
            "\"users\"",
            "\"_skippr_tombstones_users\"",
            &batch,
        )
        .unwrap();
        let script = sql.transactional_script(crate::cdc_apply::CdcWarehouseDialect::MotherDuck);

        assert!(script.starts_with("BEGIN TRANSACTION;"));
        assert!(script.contains("CREATE TEMP TABLE"));
        assert!(script.contains("'\\x02'::BLOB"));
        assert_eq!(script.matches("MERGE INTO").count(), 5);
        assert_eq!(script.matches("DROP TABLE IF EXISTS").count(), 2);
        assert!(script.ends_with("COMMIT;"));
    }

    #[test]
    fn motherduck_splits_small_http_stages_and_guards_overflow() {
        assert!(crate::cdc_apply::CdcWarehouseDialect::MotherDuck
            .validate_stage_row_count(1_000)
            .is_ok());
        assert!(crate::cdc_apply::CdcWarehouseDialect::MotherDuck
            .validate_stage_row_count(1_001)
            .unwrap_err()
            .is_warehouse_stage_limit());
        assert!(crate::cdc_apply::CdcWarehouseDialect::MotherDuck
            .validate_stage_row_count(100_000)
            .unwrap_err()
            .is_warehouse_stage_limit());
        let split_batch = crate::cdc_apply::warehouse_sql_test_batch_with_rows(251, 8);
        let split = crate::cdc_apply::warehouse_bulk_cdc_sql(
            crate::cdc_apply::CdcWarehouseDialect::MotherDuck,
            "\"users\"",
            "\"_skippr_tombstones_users\"",
            &split_batch,
        )
        .unwrap();
        assert_eq!(
            split
                .setup_statements
                .iter()
                .filter(|statement| statement.starts_with("INSERT INTO"))
                .count(),
            2
        );
        assert!(
            split
                .transactional_script(crate::cdc_apply::CdcWarehouseDialect::MotherDuck)
                .len()
                <= 512 * 1024
        );

        let oversized = crate::cdc_apply::warehouse_sql_test_batch_with_rows(1_001, 0);
        let error = crate::cdc_apply::warehouse_bulk_cdc_sql(
            crate::cdc_apply::CdcWarehouseDialect::MotherDuck,
            "\"users\"",
            "\"_skippr_tombstones_users\"",
            &oversized,
        )
        .unwrap_err();
        assert!(error.is_warehouse_stage_limit());
        let guarded = crate::cdc_apply::guarded_warehouse_cdc_sql::<super::MotherduckCdcBackend>(
            crate::cdc_apply::CdcWarehouseDialect::MotherDuck,
            "\"users\"",
            "\"_skippr_tombstones_users\"",
            &oversized,
        )
        .unwrap();
        assert_eq!(guarded.len(), 1_001);
        assert!(super::MOTHERDUCK_NATIVE_STAGE_BLOCKER.contains("no DuckDB appender"));
    }

    #[test]
    fn motherduck_insert_row_quotes_published_date_timestamp_and_binary() {
        use datafusion::arrow::array::{
            ArrayRef, BinaryArray, Date32Array, TimestampMicrosecondArray,
        };
        use datafusion::arrow::datatypes::{DataType, Field, Schema, TimeUnit};
        use datafusion::arrow::record_batch::RecordBatch;
        use std::sync::Arc;

        let batch = RecordBatch::try_new(
            Arc::new(Schema::new(vec![
                Field::new("day", DataType::Date32, true),
                Field::new("at", DataType::Timestamp(TimeUnit::Microsecond, None), true),
                Field::new("raw", DataType::Binary, true),
            ])),
            vec![
                Arc::new(Date32Array::from(vec![Some(19_723)])) as ArrayRef,
                Arc::new(TimestampMicrosecondArray::from(vec![Some(0)])),
                Arc::new(BinaryArray::from_vec(vec![b"ab".as_slice()])),
            ],
        )
        .unwrap();
        assert_eq!(
            crate::cdc_apply::sql_values_row::<super::MotherduckCdcBackend>(&batch, 0).unwrap(),
            "('2024-01-01', '1970-01-01T00:00:00', '\\x6162'::BLOB)"
        );
    }
}
