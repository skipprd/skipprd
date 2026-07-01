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
use tracing::{error, info};

static CDC_DDL_ENSURED: Lazy<DashSet<String>> = Lazy::new(DashSet::new);

pub struct SynapseCdcBackend;

impl super::cdc_apply::CdcApplyBackend for SynapseCdcBackend {
    const ORDER_TOKEN_TYPE: &'static str = "VARBINARY(MAX)";

    fn binary_literal(hex: &str) -> String {
        format!("CONVERT(VARBINARY(MAX), 0x{hex})")
    }

    fn ddl_add_order_token_column(fq_table: &str) -> String {
        format!(
            "ALTER TABLE {fq_table} ADD \"_skippr_order_token\" {}",
            Self::ORDER_TOKEN_TYPE,
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
            self.sync(chunk.into_stream(schema.clone()), chunk_ctx.filename, chunk_ctx.cdc_ctx)
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

    fn arrow_value_to_sql_synapse(col: &dyn datafusion::arrow::array::Array, row: usize) -> String {
        let val = arrow::util::display::array_value_to_string(col, row)
            .unwrap_or_else(|_| "NULL".to_string());
        if val == "NULL" || val.is_empty() {
            "NULL".to_string()
        } else {
            format!("N'{}'", val.replace('\'', "''"))
        }
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

        let fq_table = format!("\"{}\".\"{}\"", schema, table_name);

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
                    format!("\"{}\" {}", f.name(), synapse_type)
                })
                .collect();
            let create_base = format!(
                "IF NOT EXISTS (SELECT 1 FROM INFORMATION_SCHEMA.TABLES WHERE TABLE_SCHEMA = '{}' AND TABLE_NAME = '{}') \
                 CREATE TABLE {} ({})",
                schema, table_name, fq_table, col_defs.join(", ")
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

            let tombstone_tbl = tombstone_table_name(&fq_table);
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
                ddl_create_tombstone_table::<SynapseCdcBackend>(&tombstone_tbl, &bk_type_pairs);
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
                arrow_schema
                    .fields()
                    .iter()
                    .find(|f| f.name() == bk)
                    .map(|f| Self::arrow_to_synapse_type(f.data_type()))
                    .unwrap_or_else(|| "NVARCHAR(4000)".to_string())
            })
            .collect();

        let col_names_quoted: Vec<String> = arrow_schema
            .fields()
            .iter()
            .map(|f| format!("\"{}\"", f.name()))
            .collect();

        let mut row_offset = 0usize;
        let mut total_rows = 0usize;

        while let Some(batch_result) = stream.next().await {
            let batch = batch_result.map_err(|e| std::io::Error::other(e.to_string()))?;
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
                            .map(|col| {
                                Self::arrow_value_to_sql_synapse(batch.column(col).as_ref(), row)
                            })
                            .collect();
                        all_values.push(format!("CONVERT(VARBINARY(MAX), 0x{})", order_token_hex));

                        let sql = upsert_if_newer_sql::<SynapseCdcBackend>(
                            &fq_table,
                            &tombstone_table,
                            &all_names,
                            &all_values,
                            &bk_names_quoted,
                            &order_token_hex,
                        );

                        client.execute(sql.as_str(), &[]).await.map_err(|e| {
                            error!("CDC upsert failed for {}: {}", fq_table, e);
                            counters::dec_uploads_in_flight();
                            std::io::Error::other(e.to_string())
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
                                    .position(|f| f.name() == bk)
                                    .unwrap_or(0);
                                Self::arrow_value_to_sql_synapse(
                                    batch.column(col_idx).as_ref(),
                                    row,
                                )
                            })
                            .collect();

                        let sql = delete_if_newer_sql::<SynapseCdcBackend>(
                            &fq_table,
                            &tombstone_table,
                            &bk_names_quoted,
                            &bk_values,
                            &bk_types,
                            &order_token_hex,
                        );

                        client.execute(sql.as_str(), &[]).await.map_err(|e| {
                            error!("CDC delete failed for {}: {}", fq_table, e);
                            counters::dec_uploads_in_flight();
                            std::io::Error::other(e.to_string())
                        })?;
                    }
                }
            }

            row_offset += num_rows;
            total_rows += num_rows;
            counters::add_parquet_rows(num_rows as u64);
            info!(
                "CDC applied {} rows to [{}].[{}] (total: {})",
                num_rows, schema, table_name, total_rows
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
