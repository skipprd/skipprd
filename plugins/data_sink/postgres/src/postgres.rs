use dashmap::DashSet;
use datafusion::execution::SendableRecordBatchStream;
use futures::StreamExt;
use once_cell::sync::Lazy;
use tracing::{error, info};

use crate::cdc_apply::{
    append_record_batch_to_cdc_apply, apply_postgres_cdc_batch, ddl_add_order_token_column,
    ddl_create_tombstone_table, sql_values_row, tombstone_table_name, CdcApplyBatch,
    CdcApplyColumn,
};
use crate::config::DataSinkPostgresPluginConfig;
use skippr_runtime_sdk::discover::OutputMetadata;
use skippr_runtime_sdk::metrics::counters;
use skippr_runtime_sdk::plugins::alter_ddl::{
    dest_table_name, publish_warehouse_table, DestPublishCache, InstalledDestSchema,
    WarehouseDialect,
};
use skippr_runtime_sdk::plugins::cdc::SyncContext;
use skippr_runtime_sdk::sink_compat::BufferChunker;

static ENSURED_SCHEMAS: Lazy<DashSet<String>> = Lazy::new(DashSet::new);
static CDC_DDL_ENSURED: Lazy<DashSet<String>> = Lazy::new(DashSet::new);

pub struct PostgresCdcBackend;

impl crate::cdc_apply::CdcApplyBackend for PostgresCdcBackend {
    const ORDER_TOKEN_TYPE: &'static str = "BYTEA";

    fn binary_literal(hex: &str) -> String {
        format!("decode('{hex}', 'hex')")
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
        let loses =
            |alias: &str| crate::cdc_apply::sql_stored_loses_to_incoming(alias, &token, incoming);
        let tombstone_bk_match = business_key_names
            .iter()
            .map(|name| {
                let idx = all_col_names.iter().position(|n| n == name).unwrap_or(0);
                format!("{fq_tombstone_table}.{name} = {}", all_col_values[idx])
            })
            .collect::<Vec<_>>()
            .join(" AND ");
        let conflict_cols = business_key_names.join(", ");
        let update_set = all_col_names
            .iter()
            .filter(|name| {
                !business_key_names.contains(name) && name.as_str() != "\"_skippr_order_token\""
            })
            .map(|name| format!("{name} = EXCLUDED.{name}"))
            .collect::<Vec<_>>();
        let update_set_with_token = {
            let mut parts = update_set;
            parts.push("\"_skippr_order_token\" = EXCLUDED.\"_skippr_order_token\"".to_string());
            parts.push("\"_skippr_tie_seq\" = EXCLUDED.\"_skippr_tie_seq\"".to_string());
            parts.push("\"_skippr_tie_ordinal\" = EXCLUDED.\"_skippr_tie_ordinal\"".to_string());
            parts.push("\"_skippr_tie_row\" = EXCLUDED.\"_skippr_tie_row\"".to_string());
            parts.join(", ")
        };
        let bk_match = business_key_names
            .iter()
            .map(|name| format!("{fq_tombstone_table}.{name} = EXCLUDED.{name}"))
            .collect::<Vec<_>>()
            .join(" AND ");

        format!(
            "BEGIN;\n\
             INSERT INTO {fq_table} ({})\n\
             SELECT {}\n\
             WHERE NOT EXISTS (\n\
               SELECT 1 FROM {fq_tombstone_table}\n\
               WHERE {tombstone_bk_match}\n\
               AND NOT ({})\n\
             )\n\
             ON CONFLICT ({conflict_cols}) DO UPDATE SET {update_set_with_token}\n\
             WHERE ({})\n\
             AND NOT EXISTS (\n\
               SELECT 1 FROM {fq_tombstone_table}\n\
               WHERE {bk_match}\n\
               AND NOT ({})\n\
             );\n\
             DELETE FROM {fq_tombstone_table}\n\
             WHERE {tombstone_bk_match}\n\
             AND ({});\n\
             COMMIT;",
            all_col_names.join(", "),
            all_col_values.join(", "),
            loses(fq_tombstone_table),
            loses(fq_table),
            loses(fq_tombstone_table),
            loses(fq_tombstone_table),
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
        let loses =
            |alias: &str| crate::cdc_apply::sql_stored_loses_to_incoming(alias, &token, incoming);
        let delete_match = business_key_names
            .iter()
            .zip(business_key_values.iter())
            .map(|(name, value)| format!("{name} = {value}"))
            .collect::<Vec<_>>()
            .join(" AND ");
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
        let conflict_cols = business_key_names.join(", ");
        let winner_set = "\"_skippr_order_token\" = EXCLUDED.\"_skippr_order_token\", \
             \"_skippr_tie_seq\" = EXCLUDED.\"_skippr_tie_seq\", \
             \"_skippr_tie_ordinal\" = EXCLUDED.\"_skippr_tie_ordinal\", \
             \"_skippr_tie_row\" = EXCLUDED.\"_skippr_tie_row\"";

        format!(
            "BEGIN;\n\
             DELETE FROM {fq_table}\n\
             WHERE {delete_match}\n\
             AND ({});\n\
             INSERT INTO {fq_tombstone_table} ({})\n\
             SELECT {}\n\
             WHERE NOT EXISTS (\n\
               SELECT 1 FROM {fq_table}\n\
               WHERE {delete_match}\n\
               AND NOT ({})\n\
             )\n\
             ON CONFLICT ({conflict_cols}) DO UPDATE\n\
             SET {winner_set}\n\
             WHERE ({})\n\
             AND NOT EXISTS (\n\
               SELECT 1 FROM {fq_table}\n\
               WHERE {delete_match}\n\
               AND NOT ({})\n\
             );\n\
             INSERT INTO {fq_tombstone_table} ({})\n\
             SELECT {}, {fq_table}.\"_skippr_order_token\", {fq_table}.\"_skippr_tie_seq\", {fq_table}.\"_skippr_tie_ordinal\", {fq_table}.\"_skippr_tie_row\"\n\
             FROM {fq_table}\n\
             WHERE {delete_match}\n\
             ON CONFLICT ({conflict_cols}) DO UPDATE\n\
             SET {winner_set}\n\
             WHERE ({});\n\
             COMMIT;",
            loses(fq_table),
            tombstone_cols.join(", "),
            tombstone_vals.join(", "),
            loses(fq_table),
            loses(fq_tombstone_table),
            loses(fq_table),
            tombstone_cols.join(", "),
            business_key_names
                .iter()
                .map(|name| format!("{fq_table}.{name}"))
                .collect::<Vec<_>>()
                .join(", "),
            loses(fq_tombstone_table),
        )
    }
}

pub struct DataSinkPostgresPlugin {
    config: DataSinkPostgresPluginConfig,
    #[allow(dead_code)]
    buffer_name: String,
    client: tokio::sync::Mutex<Option<tokio_postgres::Client>>,
    installed: tokio::sync::RwLock<InstalledDestSchema>,
    publish_cache: DestPublishCache,
}

impl DataSinkPostgresPlugin {
    pub async fn new_with_config(
        buffer_name: String,
        config: DataSinkPostgresPluginConfig,
    ) -> Self {
        Self {
            config,
            buffer_name,
            client: tokio::sync::Mutex::new(None),
            installed: tokio::sync::RwLock::new(InstalledDestSchema::default()),
            publish_cache: DestPublishCache::default(),
        }
    }

    pub async fn sync(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
        cdc_ctx: Option<&SyncContext>,
    ) -> Result<(), std::io::Error> {
        let namespace = BufferChunker::decode_file_namespace(&filename);
        self.publish_namespace(&namespace).await?;
        match cdc_ctx {
            Some(ctx) => self.sync_cdc(stream, filename, ctx).await,
            None => self.inner_sync(stream, filename).await,
        }
    }

    pub fn installed_dest_schema(&self) -> &tokio::sync::RwLock<InstalledDestSchema> {
        &self.installed
    }

    fn fq_table(&self, namespace: &str) -> String {
        format!(
            "\"{}\".\"{}\"",
            self.config.schema,
            dest_table_name(namespace)
        )
    }

    async fn publish_namespace(&self, namespace: &str) -> Result<(), std::io::Error> {
        let (version, metadata) = self.installed.read().await.get(namespace)?;
        self.ensure_schema().await?;
        publish_warehouse_table(
            &self.publish_cache,
            self,
            WarehouseDialect::Postgres,
            &self.fq_table(namespace),
            namespace,
            version,
            &metadata,
        )
        .await
    }

    pub async fn apply_alter(
        &self,
        namespace: &str,
        _metadata: &OutputMetadata,
        alter: &skippr_runtime_sdk::discover::schema_alter::CommittedAlter,
    ) -> Result<(), std::io::Error> {
        skippr_runtime_sdk::plugins::alter_ddl::apply_warehouse_alter(
            self,
            WarehouseDialect::Postgres,
            &self.fq_table(namespace),
            namespace,
            alter,
        )
        .await
    }

    fn build_connection_string(&self) -> String {
        let mut parts = vec![
            format!("host={}", self.config.host),
            format!("port={}", self.config.port.unwrap_or(5432)),
            format!("user={}", self.config.user),
            format!("dbname={}", self.config.database),
        ];
        if let Some(ref pw) = self.config.password {
            parts.push(format!("password={}", pw));
        }
        if let Some(ref ssl) = self.config.sslmode {
            parts.push(format!("sslmode={}", ssl));
        }
        parts.join(" ")
    }

    async fn connect(&self) -> Result<tokio_postgres::Client, std::io::Error> {
        let conn_str = self.build_connection_string();
        let sslmode = self.config.sslmode.as_deref().unwrap_or("prefer");

        if sslmode == "disable" {
            let (client, connection) = tokio_postgres::connect(&conn_str, tokio_postgres::NoTls)
                .await
                .map_err(|e| std::io::Error::other(format!("Postgres connect: {}", e)))?;
            tokio::spawn(async move {
                if let Err(e) = connection.await {
                    error!("Postgres connection closed: {}", e);
                }
            });
            Ok(client)
        } else {
            let tls_connector = native_tls::TlsConnector::builder()
                .danger_accept_invalid_certs(sslmode == "prefer" || sslmode == "allow")
                .build()
                .map_err(|e| std::io::Error::other(format!("TLS init: {}", e)))?;
            let tls = postgres_native_tls::MakeTlsConnector::new(tls_connector);
            let (client, connection) = tokio_postgres::connect(&conn_str, tls)
                .await
                .map_err(|e| std::io::Error::other(format!("Postgres connect (TLS): {}", e)))?;
            tokio::spawn(async move {
                if let Err(e) = connection.await {
                    error!("Postgres connection closed: {}", e);
                }
            });
            Ok(client)
        }
    }

    async fn get_client(&self) -> Result<(), std::io::Error> {
        let mut guard = self.client.lock().await;
        if guard.is_some() {
            return Ok(());
        }
        info!(target: "postgres", "connecting to {}:{}/{}", self.config.host, self.config.port.unwrap_or(5432), self.config.database);
        let client = self.connect().await?;
        *guard = Some(client);
        info!(target: "postgres", "connected");
        Ok(())
    }

    async fn execute_sql(&self, sql: &str) -> Result<(), std::io::Error> {
        self.get_client().await?;
        let guard = self.client.lock().await;
        let client = guard.as_ref().unwrap();
        match client.batch_execute(sql).await {
            Ok(_) => Ok(()),
            Err(e) => {
                drop(guard);
                let mut guard = self.client.lock().await;
                *guard = None;
                Err(std::io::Error::other(format!("Postgres SQL error: {}", e)))
            }
        }
    }

    async fn execute_cdc_batch(
        &self,
        fq_table: &str,
        fq_tombstone_table: &str,
        batch: &CdcApplyBatch,
    ) -> Result<usize, std::io::Error> {
        self.get_client().await?;
        let result = {
            let mut guard = self.client.lock().await;
            let client = guard.as_mut().expect("Postgres client initialized");
            apply_postgres_cdc_batch(client, fq_table, fq_tombstone_table, batch).await
        };
        if result.is_err() {
            let mut guard = self.client.lock().await;
            *guard = None;
        }
        result
    }

    async fn ensure_schema(&self) -> Result<(), std::io::Error> {
        let key = format!("{}.{}", self.config.database, self.config.schema);
        if !ENSURED_SCHEMAS.insert(key.clone()) {
            return Ok(());
        }

        let ddl = format!("CREATE SCHEMA IF NOT EXISTS \"{}\"", self.config.schema);
        info!("Postgres DDL: {}", ddl);
        if let Err(e) = self.execute_sql(&ddl).await {
            ENSURED_SCHEMAS.remove(&key);
            error!("Postgres CREATE SCHEMA failed: {}", e);
            return Err(e);
        }

        Ok(())
    }

    async fn inner_sync(
        &self,
        mut stream: SendableRecordBatchStream,
        filename: String,
    ) -> Result<(), std::io::Error> {
        let namespace = BufferChunker::decode_file_namespace(&filename);
        let table_name = dest_table_name(&namespace);
        let col_defs = self.installed.read().await.warehouse_batch_columns(
            WarehouseDialect::Postgres,
            &namespace,
            &stream.schema(),
        )?;
        let fq_table = self.fq_table(&namespace);
        self.ensure_schema().await?;
        counters::inc_uploads_in_flight();

        let col_names: Vec<String> = col_defs
            .columns()
            .iter()
            .map(|(name, _)| WarehouseDialect::Postgres.quote_ident(name))
            .collect();
        let col_list = col_names.join(", ");

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

            let value_rows = (0..num_rows)
                .map(|row| sql_values_row::<PostgresCdcBackend>(&batch, row))
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

            match self.execute_sql(&insert_sql).await {
                Ok(_) => {
                    total_rows += num_rows;
                    counters::add_parquet_rows(num_rows as u64);
                    info!(
                        "Inserted {} rows into {} (total: {})",
                        num_rows, table_name, total_rows
                    );
                }
                Err(e) => {
                    error!("Postgres INSERT failed: {}", e);
                    counters::dec_uploads_in_flight();
                    return Err(e);
                }
            }
        }

        counters::add_upload(1);
        counters::dec_uploads_in_flight();
        info!(
            "Postgres sync complete: {} total rows into {}",
            total_rows, table_name
        );
        Ok(())
    }

    async fn sync_cdc(
        &self,
        mut stream: SendableRecordBatchStream,
        filename: String,
        ctx: &SyncContext,
    ) -> Result<(), std::io::Error> {
        let contract = match ctx.contract.as_ref() {
            Some(c) if !c.business_key_columns.is_empty() => c,
            _ => {
                info!(target: "postgres", "CDC context without contract or business keys; falling back to append");
                return self.inner_sync(stream, filename).await;
            }
        };

        let namespace = BufferChunker::decode_file_namespace(&filename);
        let table_name = dest_table_name(&namespace);
        let col_defs = self.installed.read().await.warehouse_batch_columns(
            WarehouseDialect::Postgres,
            &namespace,
            &stream.schema(),
        )?;
        let key_types = col_defs.key_types(&contract.business_key_columns)?;
        let fq_table = self.fq_table(&namespace);
        self.ensure_schema().await?;
        counters::inc_uploads_in_flight();

        if CDC_DDL_ENSURED.insert(fq_table.clone()) {
            let order_col_ddl = ddl_add_order_token_column::<PostgresCdcBackend>(&fq_table);
            if let Err(e) = self.execute_sql(&order_col_ddl).await {
                CDC_DDL_ENSURED.remove(&fq_table);
                counters::dec_uploads_in_flight();
                return Err(e);
            }

            let tombstone_tbl = tombstone_table_name(&fq_table);
            let tombstone_ddl =
                ddl_create_tombstone_table::<PostgresCdcBackend>(&tombstone_tbl, &key_types);
            if let Err(e) = self.execute_sql(&tombstone_ddl).await {
                CDC_DDL_ENSURED.remove(&fq_table);
                counters::dec_uploads_in_flight();
                return Err(e);
            }

            info!(target: "postgres", "CDC DDL applied for {}", fq_table);
        }

        let tombstone_table = tombstone_table_name(&fq_table);
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
        let mut row_offset = 0usize;
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
            if let Err(e) = self
                .execute_cdc_batch(&fq_table, &tombstone_table, &apply_batch)
                .await
            {
                error!("Postgres bulk CDC apply failed for {}: {}", fq_table, e);
                counters::dec_uploads_in_flight();
                return Err(e);
            }
            counters::add_parquet_rows(total_rows as u64);
            info!(
                target: "postgres",
                "CDC bulk-staged and atomically applied {} rows to {}",
                total_rows,
                table_name
            );
        }

        counters::add_upload(1);
        counters::dec_uploads_in_flight();
        info!(
            "Postgres CDC sync complete: {} total rows into {}",
            total_rows, table_name
        );
        Ok(())
    }
}

#[async_trait::async_trait]
impl skippr_runtime_sdk::plugins::alter_ddl::WarehouseExec for DataSinkPostgresPlugin {
    async fn columns(&self, namespace: &str) -> std::io::Result<Vec<String>> {
        let table_name = dest_table_name(namespace);
        self.get_client().await?;
        let guard = self.client.lock().await;
        let client = guard.as_ref().unwrap();
        let rows = client
            .query(
                "SELECT column_name FROM information_schema.columns WHERE table_schema = $1 AND table_name = $2",
                &[&self.config.schema, &table_name],
            )
            .await
            .map_err(|err| std::io::Error::other(format!("Postgres columns: {err}")))?;
        Ok(rows.iter().map(|row| row.get::<_, String>(0)).collect())
    }

    async fn execute(&self, sql: &str) -> std::io::Result<()> {
        self.execute_sql(sql).await
    }

    async fn query_first(&self, sql: &str) -> std::io::Result<Option<String>> {
        self.get_client().await?;
        let guard = self.client.lock().await;
        let client = guard.as_ref().unwrap();
        let rows = client
            .query(sql, &[])
            .await
            .map_err(|err| std::io::Error::other(format!("Postgres query: {err}")))?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        row.try_get::<_, String>(0)
            .or_else(|_| row.try_get::<_, i64>(0).map(|value| value.to_string()))
            .map(Some)
            .map_err(|err| std::io::Error::other(format!("Postgres query cell: {err}")))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn postgres_insert_row_quotes_published_date_and_encodes_binary() {
        use datafusion::arrow::array::{
            ArrayRef, BinaryArray, Date32Array, Time32MillisecondArray,
        };
        use datafusion::arrow::datatypes::{DataType, Field, Schema, TimeUnit};
        use datafusion::arrow::record_batch::RecordBatch;
        use std::sync::Arc;

        let batch = RecordBatch::try_new(
            Arc::new(Schema::new(vec![
                Field::new("day", DataType::Date32, true),
                Field::new("raw", DataType::Binary, true),
            ])),
            vec![
                Arc::new(Date32Array::from(vec![Some(19_723)])) as ArrayRef,
                Arc::new(BinaryArray::from_vec(vec![b"ab".as_slice()])),
            ],
        )
        .unwrap();
        assert_eq!(
            crate::cdc_apply::sql_values_row::<super::PostgresCdcBackend>(&batch, 0).unwrap(),
            "('2024-01-01', decode('6162', 'hex'))"
        );

        let times = RecordBatch::try_new(
            Arc::new(Schema::new(vec![Field::new(
                "t",
                DataType::Time32(TimeUnit::Millisecond),
                true,
            )])),
            vec![Arc::new(Time32MillisecondArray::from(vec![Some(0)])) as ArrayRef],
        )
        .unwrap();
        let err =
            crate::cdc_apply::sql_values_row::<super::PostgresCdcBackend>(&times, 0).unwrap_err();
        assert!(
            matches!(
                err,
                crate::cdc_apply::CdcApplyBatchError::UnsupportedArrow { .. }
            ),
            "{err}"
        );
    }
}
