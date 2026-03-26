use std::sync::Arc;

use tiberius::{Client, Config as TiberiusConfig, Row};
use tokio::net::TcpStream;
use tokio_util::compat::TokioAsyncWriteCompatExt;
use tracing::{error, info};

use crate::helpers::configuration::{Config, DataSourceMssqlPluginConfig, DataSourcePluginConfig};
use crate::helpers::offsets::{OffsetKey, OffsetTypes, Offsets};
use crate::ingest_work::{Ingest, IngestBatch, IngestTask, IngestTasks};
use async_trait::async_trait;
use crate::plugins::{DataSink, DataSource};

pub struct DataSourceMssqlPlugin {
    pub(crate) ingest: Ingest,
    pub(crate) config: DataSourceMssqlPluginConfig,
}

impl From<DataSourcePluginConfig> for DataSourceMssqlPluginConfig {
    fn from(plugin_config: DataSourcePluginConfig) -> Self {
        match plugin_config {
            DataSourcePluginConfig::Mssql(config) => config,
            _ => panic!("Invalid plugin type for MSSQL"),
        }
    }
}

impl DataSourceMssqlPlugin {
    pub async fn new() -> Self {
        let config: DataSourceMssqlPluginConfig =
            match Config::get_pipeline_input_plugin_config() {
                Ok(input_config) => input_config.into(),
                Err(_) => {
                    let connection_string =
                        Config::getenv("MSSQL_CONNECTION_STRING", "");
                    DataSourceMssqlPluginConfig {
                        connection_string,
                        tables: None,
                        batch_size_rows: None,
                        query_timeout_seconds: None,
                        format: Some("row".to_string()),
                        batch_size_bytes: None,
                        batch_size_seconds: None,
                    }
                }
            };

        DataSourceMssqlPlugin {
            ingest: Ingest::new(),
            config,
        }
    }

    async fn connect(
        config: &DataSourceMssqlPluginConfig,
    ) -> Result<Client<tokio_util::compat::Compat<TcpStream>>, Box<dyn std::error::Error>> {
        let tib_config = TiberiusConfig::from_ado_string(&config.connection_string)?;

        let tcp = TcpStream::connect(tib_config.get_addr()).await?;
        tcp.set_nodelay(true)?;

        let client = Client::connect(tib_config, tcp.compat_write()).await?;
        Ok(client)
    }

    async fn discover_tables(
        client: &mut Client<tokio_util::compat::Compat<TcpStream>>,
    ) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let sql = "SELECT TABLE_SCHEMA, TABLE_NAME \
                    FROM INFORMATION_SCHEMA.TABLES \
                    WHERE TABLE_TYPE = 'BASE TABLE' \
                    ORDER BY TABLE_SCHEMA, TABLE_NAME";
        let stream = client.simple_query(sql).await?;
        let rows: Vec<Row> = stream.into_first_result().await?;
        let mut tables = Vec::new();
        for row in rows {
            let schema: &str = row.get(0).unwrap_or("dbo");
            let table: &str = row.get(1).unwrap_or("");
            if !table.is_empty() {
                tables.push(format!("{}.{}", schema, table));
            }
        }
        Ok(tables)
    }

    async fn get_database_name(
        client: &mut Client<tokio_util::compat::Compat<TcpStream>>,
    ) -> String {
        match client.simple_query("SELECT DB_NAME()").await {
            Ok(stream) => match stream.into_first_result().await {
                Ok(rows) => {
                    if let Some(row) = rows.first() {
                        row.get::<&str, _>(0)
                            .unwrap_or("unknown")
                            .to_string()
                    } else {
                        "unknown".to_string()
                    }
                }
                Err(_) => "unknown".to_string(),
            },
            Err(_) => "unknown".to_string(),
        }
    }

    fn row_to_json(row: &Row) -> String {
        let mut map = serde_json::Map::new();
        for (i, col) in row.columns().iter().enumerate() {
            let name = col.name().to_string();
            let value = Self::column_value_to_json(row, i);
            map.insert(name, value);
        }
        serde_json::to_string(&serde_json::Value::Object(map)).unwrap_or_default()
    }

    fn column_value_to_json(row: &Row, idx: usize) -> serde_json::Value {
        match row.try_get::<&str, _>(idx) {
            Ok(Some(s)) => return serde_json::Value::String(s.to_string()),
            Ok(None) => return serde_json::Value::Null,
            Err(_) => {}
        }
        match row.try_get::<i32, _>(idx) {
            Ok(Some(v)) => return serde_json::json!(v),
            Ok(None) => return serde_json::Value::Null,
            Err(_) => {}
        }
        match row.try_get::<i64, _>(idx) {
            Ok(Some(v)) => return serde_json::json!(v),
            Ok(None) => return serde_json::Value::Null,
            Err(_) => {}
        }
        match row.try_get::<i16, _>(idx) {
            Ok(Some(v)) => return serde_json::json!(v),
            Ok(None) => return serde_json::Value::Null,
            Err(_) => {}
        }
        match row.try_get::<f64, _>(idx) {
            Ok(Some(v)) => return serde_json::json!(v),
            Ok(None) => return serde_json::Value::Null,
            Err(_) => {}
        }
        match row.try_get::<f32, _>(idx) {
            Ok(Some(v)) => return serde_json::json!(v),
            Ok(None) => return serde_json::Value::Null,
            Err(_) => {}
        }
        match row.try_get::<bool, _>(idx) {
            Ok(Some(v)) => return serde_json::json!(v),
            Ok(None) => return serde_json::Value::Null,
            Err(_) => {}
        }
        match row.try_get::<chrono::NaiveDateTime, _>(idx) {
            Ok(Some(v)) => return serde_json::Value::String(v.to_string()),
            Ok(None) => return serde_json::Value::Null,
            Err(_) => {}
        }
        match row.try_get::<chrono::NaiveDate, _>(idx) {
            Ok(Some(v)) => return serde_json::Value::String(v.to_string()),
            Ok(None) => return serde_json::Value::Null,
            Err(_) => {}
        }
        serde_json::Value::Null
    }

    pub async fn sync(
        &mut self,
        offsets: Arc<Offsets>,
        shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
    ) {
        info!("MSSQL input plugin starting sync");

        let mut client = match Self::connect(&self.config).await {
            Ok(c) => c,
            Err(e) => {
                error!("Failed to connect to MSSQL: {}", e);
                return;
            }
        };

        let db_name = Self::get_database_name(&mut client).await;

        let tables = match &self.config.tables {
            Some(t) => t.clone(),
            None => match Self::discover_tables(&mut client).await {
                Ok(t) => {
                    info!("Discovered {} tables from MSSQL", t.len());
                    t
                }
                Err(e) => {
                    error!("Failed to discover MSSQL tables: {}", e);
                    return;
                }
            },
        };

        let batch_size = self.config.batch_size_rows.unwrap_or(10_000);

        for table_fq in &tables {
            let parts: Vec<&str> = table_fq.splitn(2, '.').collect();
            let (schema, table) = if parts.len() == 2 {
                (parts[0], parts[1])
            } else {
                ("dbo", parts[0])
            };

            let namespace = format!("mssql.{}.{}.{}", db_name, schema, table);
            let offset_key = OffsetKey {
                namespace: format!("mssql:{}.{}.{}", db_name, schema, table),
                partition: table_fq.clone(),
            };

            if offsets.validate(&offset_key, OffsetTypes::Closed, 1) == Some(true) {
                info!("Skipping already-ingested table: {}", table_fq);
                continue;
            }

            info!("Ingesting table: {} -> namespace: {}", table_fq, namespace);

            let query_sql = format!("SELECT * FROM [{}].[{}]", schema, table);

            let stream = match client.simple_query(&query_sql).await {
                Ok(s) => s,
                Err(e) => {
                    error!("Failed to query table {}: {}", table_fq, e);
                    continue;
                }
            };

            let rows: Vec<Row> = match stream.into_first_result().await {
                Ok(r) => r,
                Err(e) => {
                    error!("Failed to read results for {}: {}", table_fq, e);
                    continue;
                }
            };

            info!("Read {} rows from {}", rows.len(), table_fq);

            let mut current_batch: Vec<IngestBatch> = Vec::new();
            let mut ingest_tasks = IngestTasks::new();

            for row in &rows {
                let json_str = Self::row_to_json(row);
                let bytes = json_str.len();

                current_batch.push(IngestBatch {
                    offset_key: offset_key.clone(),
                    data: json_str,
                    bytes,
                    source_uri: format!("mssql://{}/{}", db_name, table_fq),
                    namespace: Some(table.to_string()),
                });

                if current_batch.len() >= batch_size {
                    let batch = std::mem::take(&mut current_batch);
                    ingest_tasks.add(IngestTask::new(
                        batch,
                        offsets.clone(),
                        shared_output.clone(),
                    ));
                }
            }

            if !current_batch.is_empty() {
                ingest_tasks.add(IngestTask::new(
                    current_batch,
                    offsets.clone(),
                    shared_output.clone(),
                ));
            }

            self.ingest
                .ingest_file(&Arc::new(ingest_tasks), &offsets, shared_output.clone());
        }

        info!("MSSQL input plugin sync complete");
    }
}

#[async_trait]
impl DataSource for DataSourceMssqlPlugin {
    async fn sync(
        &mut self,
        offsets: Arc<Offsets>,
        output: Arc<Box<dyn DataSink + Send + Sync>>,
    ) -> Result<(), std::io::Error> {
        self.sync(offsets, output).await;
        Ok(())
    }
}
