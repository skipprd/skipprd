use dashmap::DashSet;
use datafusion::arrow::array::*;
use datafusion::arrow::datatypes::DataType as ArrowDataType;
use datafusion::execution::SendableRecordBatchStream;
use futures::StreamExt;
use once_cell::sync::Lazy;
use tracing::{error, info};

use crate::cdc_apply::{
    ddl_add_order_token_column, ddl_create_tombstone_table, delete_if_newer_sql,
    tombstone_table_name, upsert_if_newer_sql,
};
use crate::config::DataSinkPostgresPluginConfig;
use skippr_core::buffer::BufferChunker;
use skippr_core::converters::skippr_arrow::convert_skippr_to_arrow;
use skippr_core::discover::OutputMetadata;
use skippr_core::metrics::counters;
use skippr_core::plugins::cdc::{MutationKind, SyncContext};

static ENSURED_SCHEMAS: Lazy<DashSet<String>> = Lazy::new(DashSet::new);
static ENSURED_TABLES: Lazy<DashSet<String>> = Lazy::new(DashSet::new);
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
        order_token_hex: &str,
    ) -> String {
        let token = Self::binary_literal(order_token_hex);
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
               AND {fq_tombstone_table}.\"_skippr_order_token\" >= {token}\n\
             )\n\
             ON CONFLICT ({conflict_cols}) DO UPDATE SET {update_set_with_token}\n\
             WHERE (\n\
               {fq_table}.\"_skippr_order_token\" IS NULL\n\
               OR {fq_table}.\"_skippr_order_token\" < {token}\n\
             )\n\
             AND NOT EXISTS (\n\
               SELECT 1 FROM {fq_tombstone_table}\n\
               WHERE {bk_match}\n\
               AND {fq_tombstone_table}.\"_skippr_order_token\" >= {token}\n\
             );\n\
             DELETE FROM {fq_tombstone_table}\n\
             WHERE {tombstone_bk_match}\n\
             AND {fq_tombstone_table}.\"_skippr_order_token\" < {token};\n\
             COMMIT;",
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
        order_token_hex: &str,
    ) -> String {
        let token = Self::binary_literal(order_token_hex);
        let delete_match = business_key_names
            .iter()
            .zip(business_key_values.iter())
            .map(|(name, value)| format!("{name} = {value}"))
            .collect::<Vec<_>>()
            .join(" AND ");
        let tombstone_cols = business_key_names
            .iter()
            .cloned()
            .chain(std::iter::once("\"_skippr_order_token\"".to_string()))
            .collect::<Vec<_>>();
        let tombstone_vals = business_key_values
            .iter()
            .cloned()
            .chain(std::iter::once(token.clone()))
            .collect::<Vec<_>>();
        let conflict_cols = business_key_names.join(", ");

        format!(
            "BEGIN;\n\
             DELETE FROM {fq_table}\n\
             WHERE {delete_match}\n\
             AND (\n\
               {fq_table}.\"_skippr_order_token\" IS NULL\n\
               OR {fq_table}.\"_skippr_order_token\" < {token}\n\
             );\n\
             INSERT INTO {fq_tombstone_table} ({})\n\
             VALUES ({})\n\
             ON CONFLICT ({conflict_cols}) DO UPDATE\n\
             SET \"_skippr_order_token\" = EXCLUDED.\"_skippr_order_token\"\n\
             WHERE {fq_tombstone_table}.\"_skippr_order_token\" < EXCLUDED.\"_skippr_order_token\";\n\
             COMMIT;",
            tombstone_cols.join(", "),
            tombstone_vals.join(", "),
        )
    }
}

pub struct DataSinkPostgresPlugin {
    config: DataSinkPostgresPluginConfig,
    #[allow(dead_code)]
    buffer_name: String,
    client: tokio::sync::Mutex<Option<tokio_postgres::Client>>,
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
        }
    }

    pub async fn sync(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
        cdc_ctx: Option<&SyncContext>,
    ) -> Result<(), std::io::Error> {
        match cdc_ctx {
            Some(ctx) => self.sync_cdc(stream, filename, ctx).await,
            None => self.inner_sync(stream, filename).await,
        }
    }

    pub async fn sync_schema(
        &self,
        namespace: &str,
        metadata: &OutputMetadata,
    ) -> Result<(), std::io::Error> {
        self.ensure_schema().await?;

        let fields: std::collections::HashMap<String, OutputMetadata> = metadata
            .child_fields()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let arrow_schema = convert_skippr_to_arrow(Box::new(fields)).map_err(|e| {
            std::io::Error::other(format!(
                "Arrow schema conversion for '{}': {}",
                namespace, e
            ))
        })?;

        let table_name = Self::namespace_to_table_name(namespace);
        let col_defs: Vec<(String, &str)> = arrow_schema
            .fields()
            .iter()
            .map(|f| {
                (
                    f.name().to_lowercase(),
                    Self::arrow_type_to_postgres(f.data_type()),
                )
            })
            .collect();

        let fq_table = format!("\"{}\".\"{}\"", self.config.schema, table_name);

        self.ensure_table(&fq_table, &col_defs).await
    }

    fn namespace_to_table_name(namespace: &str) -> String {
        namespace.replace('.', "_").to_lowercase()
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

    fn arrow_type_to_postgres(dt: &ArrowDataType) -> &'static str {
        match dt {
            ArrowDataType::Boolean => "BOOLEAN",
            ArrowDataType::Int8 | ArrowDataType::Int16 => "SMALLINT",
            ArrowDataType::Int32 | ArrowDataType::UInt8 | ArrowDataType::UInt16 => "INTEGER",
            ArrowDataType::Int64 | ArrowDataType::UInt32 | ArrowDataType::UInt64 => "BIGINT",
            ArrowDataType::Float16 | ArrowDataType::Float32 => "REAL",
            ArrowDataType::Float64 => "DOUBLE PRECISION",
            ArrowDataType::Date32 | ArrowDataType::Date64 => "DATE",
            ArrowDataType::Timestamp(_, _) => "TIMESTAMP",
            ArrowDataType::Utf8 | ArrowDataType::LargeUtf8 => "TEXT",
            _ => "TEXT",
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
            ArrowDataType::Date32 => {
                let days = array
                    .as_any()
                    .downcast_ref::<Date32Array>()
                    .unwrap()
                    .value(row);
                let date = chrono::NaiveDate::from_num_days_from_ce_opt(days + 719_163)
                    .unwrap_or_default();
                format!("'{}'", date.format("%Y-%m-%d"))
            }
            ArrowDataType::Date64 => {
                let ms = array
                    .as_any()
                    .downcast_ref::<Date64Array>()
                    .unwrap()
                    .value(row);
                let secs = ms / 1000;
                let dt = chrono::DateTime::from_timestamp(secs, 0).unwrap_or_default();
                format!("'{}'", dt.format("%Y-%m-%d"))
            }
            ArrowDataType::Timestamp(unit, _) => {
                let ts = match unit {
                    datafusion::arrow::datatypes::TimeUnit::Second => {
                        let a = array
                            .as_any()
                            .downcast_ref::<TimestampSecondArray>()
                            .unwrap();
                        chrono::DateTime::from_timestamp(a.value(row), 0)
                    }
                    datafusion::arrow::datatypes::TimeUnit::Millisecond => {
                        let a = array
                            .as_any()
                            .downcast_ref::<TimestampMillisecondArray>()
                            .unwrap();
                        let v = a.value(row);
                        chrono::DateTime::from_timestamp(v / 1000, ((v % 1000) * 1_000_000) as u32)
                    }
                    datafusion::arrow::datatypes::TimeUnit::Microsecond => {
                        let a = array
                            .as_any()
                            .downcast_ref::<TimestampMicrosecondArray>()
                            .unwrap();
                        let v = a.value(row);
                        chrono::DateTime::from_timestamp(
                            v / 1_000_000,
                            ((v % 1_000_000) * 1000) as u32,
                        )
                    }
                    datafusion::arrow::datatypes::TimeUnit::Nanosecond => {
                        let a = array
                            .as_any()
                            .downcast_ref::<TimestampNanosecondArray>()
                            .unwrap();
                        let v = a.value(row);
                        chrono::DateTime::from_timestamp(
                            v / 1_000_000_000,
                            (v % 1_000_000_000) as u32,
                        )
                    }
                };
                let dt = ts.unwrap_or_default();
                format!("'{}'", dt.format("%Y-%m-%d %H:%M:%S%.6f"))
            }
            ArrowDataType::Utf8 => {
                let a = array.as_any().downcast_ref::<StringArray>().unwrap();
                format!("'{}'", a.value(row).replace('\'', "''"))
            }
            ArrowDataType::LargeUtf8 => {
                let a = array.as_any().downcast_ref::<LargeStringArray>().unwrap();
                format!("'{}'", a.value(row).replace('\'', "''"))
            }
            _ => {
                let a = array.as_any().downcast_ref::<StringArray>();
                match a {
                    Some(s) => format!("'{}'", s.value(row).replace('\'', "''")),
                    None => "NULL".to_string(),
                }
            }
        }
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

    async fn ensure_table(
        &self,
        fq_table: &str,
        col_defs: &[(String, &str)],
    ) -> Result<(), std::io::Error> {
        if col_defs.is_empty() {
            return Ok(());
        }

        if !ENSURED_TABLES.insert(fq_table.to_string()) {
            return Ok(());
        }

        let cols_sql: Vec<String> = col_defs
            .iter()
            .map(|(name, pg_type)| format!("\"{}\" {}", name, pg_type))
            .collect();

        let create_ddl = format!(
            "CREATE TABLE IF NOT EXISTS {} ({})",
            fq_table,
            cols_sql.join(", ")
        );
        info!("Postgres DDL: {}", create_ddl);
        if let Err(e) = self.execute_sql(&create_ddl).await {
            ENSURED_TABLES.remove(&fq_table.to_string());
            error!("Postgres CREATE TABLE failed: {}", e);
            return Err(e);
        }

        for (col_name, pg_type) in col_defs {
            let alter_ddl = format!(
                "ALTER TABLE {} ADD COLUMN IF NOT EXISTS \"{}\" {}",
                fq_table, col_name, pg_type
            );
            if let Err(e) = self.execute_sql(&alter_ddl).await {
                let msg = e.to_string();
                if !msg.contains("already exists") {
                    ENSURED_TABLES.remove(&fq_table.to_string());
                    error!("Postgres ALTER TABLE ADD COLUMN failed: {}", e);
                    return Err(e);
                }
            }
        }

        info!("Ensured table {}", fq_table);
        Ok(())
    }

    async fn inner_sync(
        &self,
        mut stream: SendableRecordBatchStream,
        filename: String,
    ) -> Result<(), std::io::Error> {
        counters::inc_uploads_in_flight();

        let namespace = BufferChunker::decode_file_namespace(&filename);
        let table_name = Self::namespace_to_table_name(&namespace);
        let arrow_schema = stream.schema();

        let col_defs: Vec<(String, &str)> = arrow_schema
            .fields()
            .iter()
            .map(|f| {
                (
                    f.name().to_lowercase(),
                    Self::arrow_type_to_postgres(f.data_type()),
                )
            })
            .collect();

        let fq_table = format!("\"{}\".\"{}\"", self.config.schema, table_name);

        self.ensure_schema().await.map_err(|e| {
            counters::dec_uploads_in_flight();
            e
        })?;
        self.ensure_table(&fq_table, &col_defs).await.map_err(|e| {
            counters::dec_uploads_in_flight();
            e
        })?;

        let col_names: Vec<String> = arrow_schema
            .fields()
            .iter()
            .map(|f| format!("\"{}\"", f.name().to_lowercase()))
            .collect();
        let col_list = col_names.join(", ");

        let mut total_rows = 0usize;
        while let Some(batch_result) = stream.next().await {
            let batch =
                batch_result.map_err(|e| std::io::Error::other(format!("stream error: {}", e)))?;
            let num_rows = batch.num_rows();
            if num_rows == 0 {
                continue;
            }

            let mut value_rows = Vec::with_capacity(num_rows);
            for row in 0..num_rows {
                let vals: Vec<String> = (0..batch.num_columns())
                    .map(|col| Self::arrow_value_to_sql(batch.column(col).as_ref(), row))
                    .collect();
                value_rows.push(format!("({})", vals.join(", ")));
            }

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

        counters::inc_uploads_in_flight();

        let namespace = BufferChunker::decode_file_namespace(&filename);
        let table_name = Self::namespace_to_table_name(&namespace);
        let arrow_schema = stream.schema();

        let col_defs: Vec<(String, &str)> = arrow_schema
            .fields()
            .iter()
            .map(|f| {
                (
                    f.name().to_lowercase(),
                    Self::arrow_type_to_postgres(f.data_type()),
                )
            })
            .collect();

        let fq_table = format!("\"{}\".\"{}\"", self.config.schema, table_name);

        self.ensure_schema().await.map_err(|e| {
            counters::dec_uploads_in_flight();
            e
        })?;
        self.ensure_table(&fq_table, &col_defs).await.map_err(|e| {
            counters::dec_uploads_in_flight();
            e
        })?;

        if CDC_DDL_ENSURED.insert(fq_table.clone()) {
            let order_col_ddl = ddl_add_order_token_column::<PostgresCdcBackend>(&fq_table);
            if let Err(e) = self.execute_sql(&order_col_ddl).await {
                CDC_DDL_ENSURED.remove(&fq_table);
                counters::dec_uploads_in_flight();
                return Err(e);
            }

            let tombstone_tbl = tombstone_table_name(&fq_table);
            let bk_type_pairs: Vec<(String, String)> = contract
                .business_key_columns
                .iter()
                .map(|bk| {
                    let pg_type = col_defs
                        .iter()
                        .find(|(name, _)| name == bk)
                        .map(|(_, t)| (*t).to_string())
                        .unwrap_or_else(|| "TEXT".to_string());
                    (bk.clone(), pg_type)
                })
                .collect();
            let tombstone_ddl =
                ddl_create_tombstone_table::<PostgresCdcBackend>(&tombstone_tbl, &bk_type_pairs);
            if let Err(e) = self.execute_sql(&tombstone_ddl).await {
                CDC_DDL_ENSURED.remove(&fq_table);
                counters::dec_uploads_in_flight();
                return Err(e);
            }

            info!(target: "postgres", "CDC DDL applied for {}", fq_table);
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
                    .unwrap_or_else(|| "TEXT".to_string())
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
                        all_values.push(format!("decode('{}', 'hex')", order_token_hex));

                        let sql = upsert_if_newer_sql::<PostgresCdcBackend>(
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

                        let sql = delete_if_newer_sql::<PostgresCdcBackend>(
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
            "Postgres CDC sync complete: {} total rows into {}",
            total_rows, table_name
        );
        Ok(())
    }
}
