use async_trait::async_trait;
use dashmap::DashSet;
use datafusion::arrow::array::*;
use datafusion::arrow::datatypes::DataType as ArrowDataType;
use datafusion::execution::SendableRecordBatchStream;
use futures::StreamExt;
use once_cell::sync::Lazy;
use tracing::{error, info};

use crate::buffer::BufferChunker;
use crate::helpers::configuration::{Config, DataOutputPostgresPluginConfig, OutputPluginConfig};
use crate::plugins::DataOutputPlugin;

static ENSURED_SCHEMAS: Lazy<DashSet<String>> = Lazy::new(DashSet::new);
static ENSURED_TABLES: Lazy<DashSet<String>> = Lazy::new(DashSet::new);

pub struct DataOutputPostgresPlugin {
    config: DataOutputPostgresPluginConfig,
    #[allow(dead_code)]
    buffer_name: String,
    client: tokio::sync::Mutex<Option<tokio_postgres::Client>>,
}

impl From<OutputPluginConfig> for DataOutputPostgresPluginConfig {
    fn from(plugin_config: OutputPluginConfig) -> Self {
        match plugin_config {
            OutputPluginConfig::Postgres(config) => config,
            _ => panic!("Invalid plugin type for Postgres"),
        }
    }
}

impl DataOutputPostgresPlugin {
    pub async fn new(buffer_name: String) -> Self {
        let config = Self::load_config();
        Self {
            config,
            buffer_name,
            client: tokio::sync::Mutex::new(None),
        }
    }

    pub async fn new_with_config(
        buffer_name: String,
        config: DataOutputPostgresPluginConfig,
    ) -> Self {
        Self {
            config,
            buffer_name,
            client: tokio::sync::Mutex::new(None),
        }
    }

    fn load_config() -> DataOutputPostgresPluginConfig {
        DataOutputPostgresPluginConfig {
            host: Config::getenv("POSTGRES_HOST", "localhost"),
            port: {
                let v = Config::getenv("POSTGRES_PORT", "5432");
                v.parse().ok()
            },
            user: Config::getenv("POSTGRES_USER", ""),
            password: {
                let v = Config::getenv("POSTGRES_PASSWORD", "");
                if v.is_empty() { None } else { Some(v) }
            },
            database: Config::getenv("POSTGRES_DATABASE", ""),
            schema: Config::getenv("POSTGRES_SCHEMA", "public"),
            sslmode: {
                let v = Config::getenv("POSTGRES_SSLMODE", "prefer");
                Some(v)
            },
            format: None,
        }
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
            let (client, connection) =
                tokio_postgres::connect(&conn_str, tokio_postgres::NoTls)
                    .await
                    .map_err(|e| {
                        std::io::Error::other(format!("Postgres connect: {}", e))
                    })?;
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
            let (client, connection) =
                tokio_postgres::connect(&conn_str, tls).await.map_err(|e| {
                    std::io::Error::other(format!("Postgres connect (TLS): {}", e))
                })?;
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
            ArrowDataType::Int64
            | ArrowDataType::UInt32
            | ArrowDataType::UInt64 => "BIGINT",
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
            ArrowDataType::Date32 => {
                let days = array.as_any().downcast_ref::<Date32Array>().unwrap().value(row);
                let date = chrono::NaiveDate::from_num_days_from_ce_opt(days + 719_163).unwrap_or_default();
                format!("'{}'", date.format("%Y-%m-%d"))
            }
            ArrowDataType::Date64 => {
                let ms = array.as_any().downcast_ref::<Date64Array>().unwrap().value(row);
                let secs = ms / 1000;
                let dt = chrono::DateTime::from_timestamp(secs, 0).unwrap_or_default();
                format!("'{}'", dt.format("%Y-%m-%d"))
            }
            ArrowDataType::Timestamp(unit, _) => {
                let ts = match unit {
                    datafusion::arrow::datatypes::TimeUnit::Second => {
                        let a = array.as_any().downcast_ref::<TimestampSecondArray>().unwrap();
                        chrono::DateTime::from_timestamp(a.value(row), 0)
                    }
                    datafusion::arrow::datatypes::TimeUnit::Millisecond => {
                        let a = array.as_any().downcast_ref::<TimestampMillisecondArray>().unwrap();
                        let v = a.value(row);
                        chrono::DateTime::from_timestamp(v / 1000, ((v % 1000) * 1_000_000) as u32)
                    }
                    datafusion::arrow::datatypes::TimeUnit::Microsecond => {
                        let a = array.as_any().downcast_ref::<TimestampMicrosecondArray>().unwrap();
                        let v = a.value(row);
                        chrono::DateTime::from_timestamp(v / 1_000_000, ((v % 1_000_000) * 1000) as u32)
                    }
                    datafusion::arrow::datatypes::TimeUnit::Nanosecond => {
                        let a = array.as_any().downcast_ref::<TimestampNanosecondArray>().unwrap();
                        let v = a.value(row);
                        chrono::DateTime::from_timestamp(v / 1_000_000_000, (v % 1_000_000_000) as u32)
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

        let ddl = format!(
            "CREATE SCHEMA IF NOT EXISTS \"{}\"",
            self.config.schema
        );
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
        use crate::metrics::counters;
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

        let fq_table = format!(
            "\"{}\".\"{}\"",
            self.config.schema, table_name
        );

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
            let batch = batch_result
                .map_err(|e| std::io::Error::other(format!("stream error: {}", e)))?;
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
}

#[async_trait]
impl DataOutputPlugin for DataOutputPostgresPlugin {
    async fn sync(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
    ) -> Result<(), std::io::Error> {
        self.inner_sync(stream, filename).await
    }
}
