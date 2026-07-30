use crate::helpers::configuration::DataSinkPluginConfig;
use async_trait::async_trait;
use dashmap::DashSet;
use datafusion::execution::SendableRecordBatchStream;
use futures::StreamExt;
use once_cell::sync::Lazy;
use serde_derive::Deserialize;
use skippr_runtime_sdk::plugins::DataSink;
use skippr_runtime_sdk::sink_compat::BufferChunker;
use tiberius::{Client, Config as TibConfig};
use tokio::net::TcpStream;
use tokio_util::compat::TokioAsyncWriteCompatExt;
use tracing::{error, info, warn};

static CDC_DDL_ENSURED: Lazy<DashSet<String>> = Lazy::new(DashSet::new);

pub struct SynapseCdcBackend;

impl super::cdc_apply::CdcApplyBackend for SynapseCdcBackend {
    const ORDER_TOKEN_TYPE: &'static str = "VARBINARY(MAX)";

    fn binary_literal(hex: &str) -> String {
        format!("CONVERT(VARBINARY(MAX), 0x{hex})")
    }

    fn tx_begin() -> &'static str {
        "BEGIN TRANSACTION;\n"
    }

    fn tx_commit() -> &'static str {
        "\nCOMMIT TRANSACTION;"
    }

    fn ddl_add_order_token_column(fq_table: &str) -> String {
        format!(
            "ALTER TABLE {fq_table} ADD [_skippr_order_token] {}",
            Self::ORDER_TOKEN_TYPE,
        )
    }

    fn ddl_create_tombstone_table(
        fq_tombstone_table: &str,
        business_key_cols: &[(String, String)],
    ) -> String {
        let mut column_defs = business_key_cols
            .iter()
            .map(|(name, target_type)| {
                format!("[{}] {target_type} NOT NULL", name.replace(']', "]]"))
            })
            .collect::<Vec<_>>();
        column_defs.push(format!(
            "[_skippr_order_token] {} NOT NULL",
            Self::ORDER_TOKEN_TYPE
        ));
        let primary_key = business_key_cols
            .iter()
            .map(|(name, _)| format!("[{}]", name.replace(']', "]]")))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "IF OBJECT_ID(N'{fq_tombstone_table}', N'U') IS NULL \
             CREATE TABLE {fq_tombstone_table} ({}, \
             PRIMARY KEY NONCLUSTERED ({primary_key}) NOT ENFORCED)",
            column_defs.join(", ")
        )
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct DataSinkSynapsePluginConfig {
    pub connection_string: String,
    pub schema: Option<String>,
    pub table: Option<String>,
    pub format: Option<String>,
}

impl TryFrom<DataSinkPluginConfig> for DataSinkSynapsePluginConfig {
    type Error = String;

    fn try_from(entry: DataSinkPluginConfig) -> Result<Self, Self::Error> {
        entry.decode_for_plugin("Synapse")
    }
}

pub struct DataSinkSynapsePlugin {
    config: DataSinkSynapsePluginConfig,
}

skippr_runtime_sdk::declare_sink_spec!(
    SynapseSinkSpec,
    DataSinkSynapsePlugin,
    skippr_runtime_sdk::plugins::cdc::sink_capabilities::SYNAPSE,
    skippr_runtime_sdk::plugins::FinalStateIdempotentApply
);

skippr_runtime_sdk::declare_schema_sink_spec!(
    SynapseSchemaSinkSpec,
    DataSinkSynapsePlugin,
    "Synapse"
);

#[async_trait]
impl DataSink for DataSinkSynapsePlugin {
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

        let tib_config = TibConfig::from_ado_string(&self.config.connection_string)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let tcp = TcpStream::connect(tib_config.get_addr())
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        tcp.set_nodelay(true)?;
        let mut client = Client::connect(tib_config, tcp.compat_write())
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let schema = self.config.schema.as_deref().unwrap_or("dbo");
        let table = self.config.table.as_deref().unwrap_or("data");

        let mut batch_stream = stream;
        while let Some(batch_result) = batch_stream.next().await {
            let batch = batch_result.map_err(|e| std::io::Error::other(e.to_string()))?;
            let schema_ref = batch.schema();

            for row_idx in 0..batch.num_rows() {
                let mut columns = Vec::new();
                let mut values = Vec::new();
                for (col_idx, field) in schema_ref.fields().iter().enumerate() {
                    let col = batch.column(col_idx);
                    let val = arrow::util::display::array_value_to_string(col, row_idx)
                        .unwrap_or_else(|_| "NULL".to_string());
                    columns.push(format!("[{}]", field.name()));
                    if val == "NULL" || val.is_empty() {
                        values.push("NULL".to_string());
                    } else {
                        values.push(format!("N'{}'", val.replace('\'', "''")));
                    }
                }

                let sql = format!(
                    "INSERT INTO [{}].[{}] ({}) VALUES ({})",
                    schema,
                    table,
                    columns.join(", "),
                    values.join(", ")
                );
                client
                    .execute(&sql, &[])
                    .await
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
            }
        }

        info!("Synapse: inserted rows into [{}].[{}]", schema, table);
        counters::dec_uploads_in_flight();
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
        &skippr_runtime_sdk::plugins::cdc::sink_capabilities::SYNAPSE
    }
}

impl DataSinkSynapsePlugin {
    pub async fn new_with_config(
        _buffer_name: String,
        config: DataSinkSynapsePluginConfig,
    ) -> Self {
        Self { config }
    }

    fn namespace_to_table_name(namespace: &str) -> String {
        namespace.replace('.', "_").to_lowercase()
    }

    fn arrow_to_synapse_type(dt: &arrow::datatypes::DataType) -> String {
        use arrow::datatypes::DataType;
        match dt {
            DataType::Boolean => "BIT".to_string(),
            DataType::Int8 | DataType::UInt8 => "TINYINT".to_string(),
            DataType::Int16 | DataType::UInt16 => "SMALLINT".to_string(),
            DataType::Int32 | DataType::UInt32 => "INT".to_string(),
            DataType::Int64 | DataType::UInt64 => "BIGINT".to_string(),
            DataType::Float16 | DataType::Float32 => "REAL".to_string(),
            DataType::Float64 => "FLOAT".to_string(),
            DataType::Date32 | DataType::Date64 => "DATE".to_string(),
            DataType::Timestamp(_, _) => "DATETIME2".to_string(),
            DataType::Utf8 | DataType::LargeUtf8 => "NVARCHAR(4000)".to_string(),
            DataType::Binary | DataType::LargeBinary => "VARBINARY(MAX)".to_string(),
            _ => "NVARCHAR(4000)".to_string(),
        }
    }

    async fn sync_cdc(
        &self,
        mut stream: SendableRecordBatchStream,
        filename: String,
        ctx: &skippr_runtime_sdk::plugins::cdc::SyncContext,
    ) -> Result<(), std::io::Error> {
        use super::cdc_apply::{
            append_record_batch_to_cdc_apply, ddl_add_order_token_column,
            ddl_create_tombstone_table, guarded_warehouse_cdc_row_sql, warehouse_bulk_cdc_sql,
            CdcApplyBatch, CdcApplyColumn, CdcWarehouseDialect,
        };
        use skippr_runtime_sdk::metrics::counters;

        let contract = match ctx.contract.as_ref() {
            Some(c) if !c.business_key_columns.is_empty() => c,
            _ => {
                info!(target: "synapse", "CDC context without contract or business keys; falling back to append");
                return self.sync(stream, filename, None).await;
            }
        };

        counters::inc_uploads_in_flight();

        let namespace = BufferChunker::decode_file_namespace(&filename);
        let table_name_owned = self
            .config
            .table
            .clone()
            .unwrap_or_else(|| Self::namespace_to_table_name(&namespace));
        let table_name = table_name_owned.as_str();
        let schema = self.config.schema.as_deref().unwrap_or("dbo");
        let arrow_schema = stream.schema();

        let quoted_schema = format!("[{}]", schema.replace(']', "]]"));
        let quoted_table = format!("[{}]", table_name.replace(']', "]]"));
        let fq_table = format!("{quoted_schema}.{quoted_table}");
        let tombstone_table = format!(
            "{}.[_skippr_tombstones_{}]",
            quoted_schema,
            table_name.replace(']', "]]")
        );

        let tib_config = TibConfig::from_ado_string(&self.config.connection_string)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let tcp = TcpStream::connect(tib_config.get_addr())
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        tcp.set_nodelay(true)?;
        let mut client = Client::connect(tib_config, tcp.compat_write())
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        if CDC_DDL_ENSURED.insert(fq_table.clone()) {
            // Ensure the base table exists before adding CDC columns.
            let col_defs: Vec<String> = arrow_schema
                .fields()
                .iter()
                .map(|f| {
                    let synapse_type = Self::arrow_to_synapse_type(f.data_type());
                    format!("[{}] {}", f.name().replace(']', "]]"), synapse_type)
                })
                .collect();
            let create_base = format!(
                "IF NOT EXISTS (SELECT 1 FROM INFORMATION_SCHEMA.TABLES WHERE TABLE_SCHEMA = '{}' AND TABLE_NAME = '{}') \
                 CREATE TABLE {} ({})",
                schema.replace('\'', "''"),
                table_name.replace('\'', "''"),
                fq_table,
                col_defs.join(", ")
            );
            if let Err(e) = client.execute(create_base.as_str(), &[]).await {
                let msg = e.to_string();
                if !msg.contains("already exists") && !msg.contains("already an object") {
                    CDC_DDL_ENSURED.remove(&fq_table);
                    counters::dec_uploads_in_flight();
                    return Err(std::io::Error::other(msg));
                }
            }

            let order_col_ddl = ddl_add_order_token_column::<SynapseCdcBackend>(&fq_table);
            if let Err(e) = client.execute(order_col_ddl.as_str(), &[]).await {
                let msg = e.to_string();
                if !msg.contains("already exists") && !msg.contains("Column names") {
                    CDC_DDL_ENSURED.remove(&fq_table);
                    counters::dec_uploads_in_flight();
                    return Err(std::io::Error::other(msg));
                }
            }

            let bk_type_pairs: Vec<(String, String)> = contract
                .business_key_columns
                .iter()
                .map(|bk| {
                    let arrow_type = arrow_schema
                        .fields()
                        .iter()
                        .find(|f| f.name() == bk)
                        .map(|f| Self::arrow_to_synapse_type(f.data_type()))
                        .unwrap_or_else(|| "NVARCHAR(4000)".to_string());
                    (bk.clone(), arrow_type)
                })
                .collect();
            let tombstone_ddl =
                ddl_create_tombstone_table::<SynapseCdcBackend>(&tombstone_table, &bk_type_pairs);
            if let Err(e) = client.execute(tombstone_ddl.as_str(), &[]).await {
                let msg = e.to_string();
                if !msg.contains("already exists") && !msg.contains("already an object") {
                    CDC_DDL_ENSURED.remove(&fq_table);
                    counters::dec_uploads_in_flight();
                    return Err(std::io::Error::other(msg));
                }
            }

            info!(target: "synapse", "CDC DDL applied for {}", fq_table);
        }

        let mut row_offset = 0usize;
        let mut apply_batch = CdcApplyBatch {
            columns: arrow_schema
                .fields()
                .iter()
                .map(|field| CdcApplyColumn {
                    name: field.name().clone(),
                    target_type: Self::arrow_to_synapse_type(field.data_type()),
                })
                .collect(),
            business_key_columns: contract.business_key_columns.clone(),
            rows: Vec::new(),
        };

        while let Some(batch_result) = stream.next().await {
            let batch = batch_result.map_err(|e| std::io::Error::other(e.to_string()))?;
            let num_rows = batch.num_rows();
            if num_rows == 0 {
                continue;
            }
            append_record_batch_to_cdc_apply(
                &mut apply_batch,
                &batch,
                &ctx.part_meta.rows,
                row_offset,
            )
            .map_err(|error| std::io::Error::other(error.to_string()))?;
            row_offset += num_rows;
        }

        let total_rows = apply_batch.rows.len();
        if total_rows > 0 {
            match warehouse_bulk_cdc_sql(
                CdcWarehouseDialect::Synapse,
                &fq_table,
                &tombstone_table,
                &apply_batch,
            ) {
                Ok(sql) => {
                    client
                        .execute(
                            sql.transactional_script(CdcWarehouseDialect::Synapse)
                                .as_str(),
                            &[],
                        )
                        .await
                        .map_err(|error| {
                            error!("Synapse bulk CDC apply failed for {}: {}", fq_table, error);
                            counters::dec_uploads_in_flight();
                            std::io::Error::other(error.to_string())
                        })?;
                }
                Err(error) if error.is_warehouse_stage_limit() => {
                    warn!(
                        target: "synapse",
                        "CDC chunk exceeds 1000-row/64 MiB staging envelope; using guarded row apply: {}",
                        error
                    );
                    for row in &apply_batch.rows {
                        let statement = guarded_warehouse_cdc_row_sql::<SynapseCdcBackend>(
                            CdcWarehouseDialect::Synapse,
                            &fq_table,
                            &tombstone_table,
                            &apply_batch,
                            row,
                        )
                        .map_err(|error| std::io::Error::other(error.to_string()))?;
                        client
                            .execute(statement.as_str(), &[])
                            .await
                            .map_err(|error| {
                                counters::dec_uploads_in_flight();
                                std::io::Error::other(error.to_string())
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
                target: "synapse",
                "CDC safely applied {} rows to [{}].[{}]",
                total_rows,
                schema,
                table_name
            );
        }

        counters::add_upload(1);
        counters::dec_uploads_in_flight();
        info!(
            "Synapse CDC sync complete: {} total rows into [{}].[{}]",
            total_rows, schema, table_name
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn synapse_bulk_cdc_sql_keeps_temp_ddl_outside_target_transaction() {
        let batch = crate::cdc_apply::warehouse_sql_test_batch();
        let sql = crate::cdc_apply::warehouse_bulk_cdc_sql(
            crate::cdc_apply::CdcWarehouseDialect::Synapse,
            "[dbo].[users]",
            "[dbo].[_skippr_tombstones_users]",
            &batch,
        )
        .unwrap();
        let script = sql.transactional_script(crate::cdc_apply::CdcWarehouseDialect::Synapse);
        let create_position = script.find("CREATE TABLE [#").unwrap();
        let transaction_position = script.find("BEGIN TRANSACTION").unwrap();

        assert!(create_position < transaction_position);
        assert!(script.contains("WITH (DISTRIBUTION = ROUND_ROBIN, HEAP)"));
        assert!(script.contains("CONVERT(VARBINARY(MAX), 0x"));
        assert_eq!(script.matches("MERGE INTO").count(), 4);
        assert!(script.contains("ROLLBACK TRANSACTION"));
        assert!(script.matches("DROP TABLE IF EXISTS").count() >= 4);
    }

    #[test]
    fn synapse_splits_values_rows_and_falls_back_before_batch_ceiling() {
        assert!(crate::cdc_apply::CdcWarehouseDialect::Synapse
            .validate_stage_row_count(100_000)
            .is_ok());
        assert!(crate::cdc_apply::CdcWarehouseDialect::Synapse
            .validate_stage_row_count(100_001)
            .unwrap_err()
            .is_warehouse_stage_limit());
        let split_batch = crate::cdc_apply::warehouse_sql_test_batch_with_rows(1_001, 8);
        let split = crate::cdc_apply::warehouse_bulk_cdc_sql(
            crate::cdc_apply::CdcWarehouseDialect::Synapse,
            "[dbo].[users]",
            "[dbo].[_skippr_tombstones_users]",
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
                .transactional_script(crate::cdc_apply::CdcWarehouseDialect::Synapse)
                .len()
                <= 64 * 1024 * 1024
        );

        let hundred_thousand = crate::cdc_apply::warehouse_sql_test_batch_with_rows(100_000, 0);
        let hundred_thousand_sql = crate::cdc_apply::warehouse_bulk_cdc_sql(
            crate::cdc_apply::CdcWarehouseDialect::Synapse,
            "[dbo].[users]",
            "[dbo].[_skippr_tombstones_users]",
            &hundred_thousand,
        )
        .unwrap();
        assert_eq!(
            hundred_thousand_sql
                .setup_statements
                .iter()
                .filter(|statement| statement.starts_with("INSERT INTO"))
                .count(),
            100
        );

        let oversized = crate::cdc_apply::warehouse_sql_test_batch_with_rows(1, 2 * 1024 * 1024);
        let error = crate::cdc_apply::warehouse_bulk_cdc_sql(
            crate::cdc_apply::CdcWarehouseDialect::Synapse,
            "[dbo].[users]",
            "[dbo].[_skippr_tombstones_users]",
            &oversized,
        )
        .unwrap_err();
        assert!(error.is_warehouse_stage_limit());
        let guarded = crate::cdc_apply::guarded_warehouse_cdc_sql::<super::SynapseCdcBackend>(
            crate::cdc_apply::CdcWarehouseDialect::Synapse,
            "[dbo].[users]",
            "[dbo].[_skippr_tombstones_users]",
            &oversized,
        )
        .unwrap();
        assert_eq!(guarded.len(), 1);
        assert!(guarded[0].starts_with("BEGIN TRANSACTION;"));
        assert!(guarded[0].ends_with("COMMIT TRANSACTION;"));
    }
}
