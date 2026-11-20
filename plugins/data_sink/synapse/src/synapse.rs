use crate::helpers::configuration::DataSinkPluginConfig;
use async_trait::async_trait;
use dashmap::DashSet;
use datafusion::execution::SendableRecordBatchStream;
use futures::StreamExt;
use once_cell::sync::Lazy;
use serde_derive::Deserialize;
use skippr_runtime_sdk::discover::{OutputMetadata, SkipprDataType};
use skippr_runtime_sdk::plugins::alter_ddl::{
    dest_table_name, missing_published_columns, published_columns_folded,
    require_published_columns_live, DestPublishCache, IdentCase, InstalledDestSchema,
    PublishedBatchColumns,
};
use skippr_runtime_sdk::plugins::DataSink;
use skippr_runtime_sdk::sink_compat::BufferChunker;
use skippr_runtime_sdk::SkipprConfig;
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

    fn string_literal(value: &str) -> String {
        super::cdc_apply::nvarchar_string_literal(value)
    }

    fn boolean_literal(value: bool) -> &'static str {
        if value {
            "1"
        } else {
            "0"
        }
    }

    fn tx_begin() -> &'static str {
        "BEGIN TRANSACTION;\n"
    }

    fn tx_commit() -> &'static str {
        "\nCOMMIT TRANSACTION;"
    }

    fn ddl_add_order_token_column(fq_table: &str) -> String {
        format!(
            "ALTER TABLE {fq_table} ADD [_skippr_order_token] {};\n\
             ALTER TABLE {fq_table} ADD [_skippr_tie_seq] BIGINT;\n\
             ALTER TABLE {fq_table} ADD [_skippr_tie_ordinal] INT;\n\
             ALTER TABLE {fq_table} ADD [_skippr_tie_row] BIGINT",
            Self::ORDER_TOKEN_TYPE,
        )
    }

    fn ddl_create_tombstone_table(
        fq_tombstone_table: &str,
        business_key_cols: &[(String, &str)],
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
        column_defs.push("[_skippr_tie_seq] BIGINT NOT NULL".to_string());
        column_defs.push("[_skippr_tie_ordinal] INT NOT NULL".to_string());
        column_defs.push("[_skippr_tie_row] BIGINT NOT NULL".to_string());
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

#[derive(Debug, Deserialize, SkipprConfig, Clone)]
pub struct DataSinkSynapsePluginConfig {
    #[skippr(secret)]
    pub connection_string: String,
    pub schema: Option<String>,
    pub table: Option<String>,
}

impl TryFrom<DataSinkPluginConfig> for DataSinkSynapsePluginConfig {
    type Error = String;

    fn try_from(entry: DataSinkPluginConfig) -> Result<Self, Self::Error> {
        entry.decode_for_plugin("Synapse")
    }
}

pub struct DataSinkSynapsePlugin {
    config: DataSinkSynapsePluginConfig,
    installed: tokio::sync::RwLock<InstalledDestSchema>,
    publish_cache: DestPublishCache,
}

skippr_runtime_sdk::declare_sink_spec!(
    SynapseSinkSpec,
    DataSinkSynapsePlugin,
    skippr_runtime_sdk::plugins::cdc::sink_capabilities::SYNAPSE,
    skippr_runtime_sdk::plugins::FinalStateIdempotentApply
);

#[async_trait]
impl DataSink for DataSinkSynapsePlugin {
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
        &skippr_runtime_sdk::plugins::cdc::sink_capabilities::SYNAPSE
    }

    fn installed_dest_schema(&self) -> Option<&tokio::sync::RwLock<InstalledDestSchema>> {
        Some(&self.installed)
    }
}

impl DataSinkSynapsePlugin {
    pub async fn new_with_config(
        _buffer_name: String,
        config: DataSinkSynapsePluginConfig,
    ) -> Self {
        Self {
            config,
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

    fn schema(&self) -> &str {
        self.config.schema.as_deref().unwrap_or("dbo")
    }

    fn fq_table(&self, namespace: &str) -> String {
        format!(
            "{}.{}",
            Self::bracket(self.schema()),
            Self::bracket(&self.dest_table(namespace))
        )
    }

    fn sql_type(ty: &SkipprDataType) -> Result<&'static str, std::io::Error> {
        Ok(match ty {
            SkipprDataType::Boolean => "BIT",
            SkipprDataType::Byte => "TINYINT",
            SkipprDataType::Short => "SMALLINT",
            SkipprDataType::Integer => "INT",
            SkipprDataType::Long => "BIGINT",
            SkipprDataType::Float => "REAL",
            SkipprDataType::Double => "FLOAT",
            SkipprDataType::Date => "DATE",
            SkipprDataType::Timestamp | SkipprDataType::TimestampMilli => "DATETIME2",
            SkipprDataType::String | SkipprDataType::Uuid | SkipprDataType::Json => {
                "NVARCHAR(4000)"
            }
            SkipprDataType::Binary | SkipprDataType::Fixed => "VARBINARY(MAX)",
            other => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("Synapse publish has no SQL type for {other}"),
                ))
            }
        })
    }

    fn bracket(name: &str) -> String {
        format!("[{}]", name.replace(']', "]]"))
    }

    fn publish_sql(
        schema: &str,
        table_name: &str,
        metadata: &OutputMetadata,
        existing: &[String],
    ) -> Result<Vec<String>, std::io::Error> {
        let columns = published_columns_folded(metadata, IdentCase::Lower)?;
        let quoted_schema = Self::bracket(schema);
        let quoted_table = Self::bracket(table_name);
        let fq_table = format!("{quoted_schema}.{quoted_table}");
        if existing.is_empty() {
            let defs = columns
                .iter()
                .map(|column| {
                    Ok(format!(
                        "{} {}",
                        Self::bracket(&column.name),
                        Self::sql_type(&column.ty)?
                    ))
                })
                .collect::<Result<Vec<_>, std::io::Error>>()?;
            return Ok(vec![format!(
                "IF NOT EXISTS (SELECT 1 FROM INFORMATION_SCHEMA.TABLES WHERE TABLE_SCHEMA = '{}' AND TABLE_NAME = '{}') CREATE TABLE {} ({})",
                schema.replace('\'', "''"),
                table_name.replace('\'', "''"),
                fq_table,
                defs.join(", ")
            )]);
        }
        missing_published_columns(&columns, existing, IdentCase::Lower)
            .into_iter()
            .map(|column| {
                let col = column.name.replace('\'', "''");
                Ok(format!(
                    "IF COL_LENGTH('{}.{}', '{}') IS NULL ALTER TABLE {} ADD {} {}",
                    schema.replace('\'', "''"),
                    table_name.replace('\'', "''"),
                    col,
                    fq_table,
                    Self::bracket(&column.name),
                    Self::sql_type(&column.ty)?
                ))
            })
            .collect()
    }

    async fn connect(
        &self,
    ) -> Result<Client<tokio_util::compat::Compat<TcpStream>>, std::io::Error> {
        let tib_config = TibConfig::from_ado_string(&self.config.connection_string)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let tcp = TcpStream::connect(tib_config.get_addr())
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        tcp.set_nodelay(true)?;
        Client::connect(tib_config, tcp.compat_write())
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))
    }

    async fn table_columns(
        &self,
        client: &mut Client<tokio_util::compat::Compat<TcpStream>>,
        schema: &str,
        table_name: &str,
    ) -> Result<Vec<String>, std::io::Error> {
        let sql = format!(
            "SELECT COLUMN_NAME FROM INFORMATION_SCHEMA.COLUMNS WHERE TABLE_SCHEMA = '{}' AND TABLE_NAME = '{}'",
            schema.replace('\'', "''"),
            table_name.replace('\'', "''"),
        );
        let rows = client
            .query(sql, &[])
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?
            .into_first_result()
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let mut names = Vec::new();
        for row in rows {
            let name: &str = row
                .get(0)
                .ok_or_else(|| std::io::Error::other("Synapse column row has no name"))?;
            names.push(name.to_string());
        }
        Ok(names)
    }

    async fn publish_namespace(&self, namespace: &str) -> Result<(), std::io::Error> {
        let (version, metadata) = self.installed.read().await.get(namespace)?;
        self.publish_cache
            .publish(namespace, version, || async {
                let table_name = self.dest_table(namespace);
                let mut client = self.connect().await?;
                let existing = self
                    .table_columns(&mut client, self.schema(), &table_name)
                    .await?;
                for sql in Self::publish_sql(self.schema(), &table_name, &metadata, &existing)? {
                    client
                        .execute(sql.as_str(), &[])
                        .await
                        .map_err(|e| std::io::Error::other(e.to_string()))?;
                }
                let live = self
                    .table_columns(&mut client, self.schema(), &table_name)
                    .await?;
                require_published_columns_live(
                    &published_columns_folded(&metadata, IdentCase::Lower)?,
                    &live,
                    IdentCase::Lower,
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
        let col_defs = self.batch_columns(&namespace, &stream.schema()).await?;
        let fq_table = self.fq_table(&namespace);
        let col_list = col_defs
            .columns()
            .iter()
            .map(|(name, _)| Self::bracket(name))
            .collect::<Vec<_>>()
            .join(", ");
        let mut client = self.connect().await?;
        counters::inc_uploads_in_flight();
        while let Some(batch_result) = stream.next().await {
            let batch = batch_result.map_err(|e| {
                counters::dec_uploads_in_flight();
                std::io::Error::other(e.to_string())
            })?;
            for row in 0..batch.num_rows() {
                let values = crate::cdc_apply::sql_values_row::<SynapseCdcBackend>(&batch, row)
                    .map_err(|e| {
                        counters::dec_uploads_in_flight();
                        std::io::Error::from(e)
                    })?;
                let sql = format!("INSERT INTO {fq_table} ({col_list}) VALUES {values}");
                client.execute(&sql, &[]).await.map_err(|e| {
                    counters::dec_uploads_in_flight();
                    std::io::Error::other(e.to_string())
                })?;
            }
        }
        info!("Synapse: inserted rows into {}", fq_table);
        counters::dec_uploads_in_flight();
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
            ddl_create_tombstone_table, guarded_warehouse_cdc_row_sql, warehouse_bulk_cdc_sql,
            CdcApplyBatch, CdcApplyColumn, CdcWarehouseDialect,
        };
        use skippr_runtime_sdk::metrics::counters;

        let contract = match ctx.contract.as_ref() {
            Some(c) if !c.business_key_columns.is_empty() => c,
            _ => {
                info!(target: "synapse", "CDC context without contract or business keys; falling back to append");
                return self.sync_append(stream, &filename).await;
            }
        };

        let namespace = BufferChunker::decode_file_namespace(&filename);
        let table_name = self.dest_table(&namespace);
        let col_defs = self.batch_columns(&namespace, &stream.schema()).await?;
        let key_types = col_defs.key_types(&contract.business_key_columns)?;
        let fq_table = self.fq_table(&namespace);
        let tombstone_table = format!(
            "{}.[_skippr_tombstones_{}]",
            Self::bracket(self.schema()),
            table_name.replace(']', "]]")
        );
        counters::inc_uploads_in_flight();

        let mut client = self.connect().await.map_err(|e| {
            counters::dec_uploads_in_flight();
            e
        })?;

        if CDC_DDL_ENSURED.insert(fq_table.clone()) {
            let order_col_ddl = ddl_add_order_token_column::<SynapseCdcBackend>(&fq_table);
            if let Err(e) = client.execute(order_col_ddl.as_str(), &[]).await {
                let msg = e.to_string();
                if !msg.contains("already exists") && !msg.contains("Column names") {
                    CDC_DDL_ENSURED.remove(&fq_table);
                    counters::dec_uploads_in_flight();
                    return Err(std::io::Error::other(msg));
                }
            }

            let tombstone_ddl =
                ddl_create_tombstone_table::<SynapseCdcBackend>(&tombstone_table, &key_types);
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
                std::io::Error::other(e.to_string())
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
                    let collapsed = collapse_apply_batch(&apply_batch);
                    for row in &collapsed.rows {
                        let statement = guarded_warehouse_cdc_row_sql::<SynapseCdcBackend>(
                            CdcWarehouseDialect::Synapse,
                            &fq_table,
                            &tombstone_table,
                            &collapsed,
                            row,
                        )?;
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
                "CDC safely applied {} rows to {}",
                total_rows,
                fq_table
            );
        }

        counters::add_upload(1);
        counters::dec_uploads_in_flight();
        info!(
            "Synapse CDC sync complete: {} total rows into {}",
            total_rows, fq_table
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
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
            super::DataSinkSynapsePlugin::publish_sql("dbo", "trips", &metadata, &[]).unwrap();
        assert_eq!(
            created,
            vec![
                "IF NOT EXISTS (SELECT 1 FROM INFORMATION_SCHEMA.TABLES WHERE TABLE_SCHEMA = 'dbo' AND TABLE_NAME = 'trips') CREATE TABLE [dbo].[trips] ([id] BIGINT, [region] NVARCHAR(4000))"
            ]
        );
        let added = super::DataSinkSynapsePlugin::publish_sql(
            "dbo",
            "trips",
            &metadata,
            &["id".to_string()],
        )
        .unwrap();
        assert_eq!(
            added,
            vec!["IF COL_LENGTH('dbo.trips', 'region') IS NULL ALTER TABLE [dbo].[trips] ADD [region] NVARCHAR(4000)"]
        );
        assert!(added.iter().all(|sql| !sql.contains("CREATE TABLE")));
        let err = super::DataSinkSynapsePlugin::sql_type(&SkipprDataType::Record).unwrap_err();
        assert!(err.to_string().contains("no SQL type"), "{err}");
    }

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
        assert_eq!(script.matches("MERGE INTO").count(), 5);
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

    #[test]
    fn synapse_insert_row_uses_unicode_text_bit_booleans_and_varbinary() {
        use datafusion::arrow::array::{
            ArrayRef, BinaryArray, BooleanArray, Date32Array, StringArray,
        };
        use datafusion::arrow::datatypes::{DataType, Field, Schema};
        use datafusion::arrow::record_batch::RecordBatch;
        use std::sync::Arc;

        let batch = RecordBatch::try_new(
            Arc::new(Schema::new(vec![
                Field::new("day", DataType::Date32, true),
                Field::new("name", DataType::Utf8, true),
                Field::new("empty", DataType::Utf8, true),
                Field::new("flag", DataType::Boolean, true),
                Field::new("raw", DataType::Binary, true),
            ])),
            vec![
                Arc::new(Date32Array::from(vec![Some(19_723)])) as ArrayRef,
                Arc::new(StringArray::from(vec![Some("é'")])),
                Arc::new(StringArray::from(vec![Some("")])),
                Arc::new(BooleanArray::from(vec![Some(false)])),
                Arc::new(BinaryArray::from_vec(vec![b"ab".as_slice()])),
            ],
        )
        .unwrap();
        assert_eq!(
            crate::cdc_apply::sql_values_row::<super::SynapseCdcBackend>(&batch, 0).unwrap(),
            "(N'2024-01-01', N'é''', N'', 0, CONVERT(VARBINARY(MAX), 0x6162))"
        );
    }
}
