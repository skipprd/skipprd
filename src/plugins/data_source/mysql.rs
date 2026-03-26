use std::sync::Arc;

use async_trait::async_trait;
use mysql_async::prelude::*;
use mysql_async::{Pool, Row, Value as MysqlValue};
use serde_derive::Deserialize;
use serde_json::{json, Map, Value};
use tracing::{error, info};

use crate::helpers::configuration::{Config, DataSourcePluginConfig};
use crate::helpers::offsets::{OffsetKey, OffsetTypes, Offsets};
use crate::ingest_work::{Ingest, IngestBatch, IngestTask, IngestTasks};
use crate::plugins::{DataSink, DataSource};

#[derive(Debug, Deserialize, Clone)]
pub struct DataSourceMysqlPluginConfig {
    pub connection_string: String,
    pub tables: Option<Vec<String>>,
    pub format: Option<String>,
    pub batch_size_bytes: Option<i64>,
    pub batch_size_seconds: Option<i64>,
}

impl From<DataSourcePluginConfig> for DataSourceMysqlPluginConfig {
    fn from(plugin_config: DataSourcePluginConfig) -> Self {
        match plugin_config {
            DataSourcePluginConfig::Mysql(config) => config,
            _ => panic!("Invalid plugin type for MySQL"),
        }
    }
}

pub struct DataSourceMysqlPlugin {
    pub(crate) ingest: Ingest,
    pub(crate) config: DataSourceMysqlPluginConfig,
}

impl DataSourceMysqlPlugin {
    pub async fn new() -> Self {
        let config: DataSourceMysqlPluginConfig =
            match Config::get_pipeline_input_plugin_config() {
                Ok(input_config) => input_config.into(),
                Err(_) => {
                    let connection_string =
                        Config::getenv("MYSQL_CONNECTION_STRING", "");
                    DataSourceMysqlPluginConfig {
                        connection_string,
                        tables: None,
                        format: Some("row".to_string()),
                        batch_size_bytes: None,
                        batch_size_seconds: None,
                    }
                }
            };

        DataSourceMysqlPlugin {
            ingest: Ingest::new(),
            config,
        }
    }

    async fn connect_pool(
        config: &DataSourceMysqlPluginConfig,
    ) -> Result<(Pool, mysql_async::Conn), mysql_async::Error> {
        let pool = Pool::new(config.connection_string.as_str());
        let conn = pool.get_conn().await?;
        Ok((pool, conn))
    }

    async fn discover_tables(conn: &mut mysql_async::Conn) -> Result<Vec<String>, mysql_async::Error> {
        let sql = "SELECT TABLE_SCHEMA, TABLE_NAME FROM INFORMATION_SCHEMA.TABLES \
                   WHERE TABLE_TYPE = 'BASE TABLE' ORDER BY TABLE_SCHEMA, TABLE_NAME";
        let rows: Vec<Row> = conn.query(sql).await?;
        let mut tables = Vec::new();
        for row in rows {
            let schema: String = row
                .get(0)
                .unwrap_or_else(|| "".to_string());
            let table: String = row.get(1).unwrap_or_else(|| "".to_string());
            if !table.is_empty() {
                tables.push(format!("{}.{}", schema, table));
            }
        }
        Ok(tables)
    }

    async fn get_database_name(conn: &mut mysql_async::Conn) -> String {
        let sql = "SELECT DATABASE()";
        match conn.query_first::<Row, _>(sql).await {
            Ok(Some(row)) => match row.get::<Option<String>, _>(0) {
                Some(Some(s)) if !s.is_empty() => s,
                _ => "unknown".to_string(),
            },
            _ => "unknown".to_string(),
        }
    }

    fn escape_ident(ident: &str) -> String {
        format!("`{}`", ident.replace('`', "``"))
    }

    fn mysql_value_to_json(v: &MysqlValue) -> Value {
        match v {
            MysqlValue::NULL => Value::Null,
            MysqlValue::Bytes(b) => Value::String(String::from_utf8_lossy(b).into_owned()),
            MysqlValue::Int(i) => json!(i),
            MysqlValue::UInt(u) => json!(u),
            MysqlValue::Float(f) => json!(f),
            MysqlValue::Double(d) => json!(d),
            MysqlValue::Date(y, mo, d, h, mi, s, micro) => {
                Value::String(format!(
                    "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:06}",
                    y, mo, d, h, mi, s, micro
                ))
            }
            MysqlValue::Time(neg, days, h, mi, s, micro) => {
                let sign = if *neg { "-" } else { "" };
                Value::String(format!(
                    "{}{} {}:{:02}:{:02}.{:06}",
                    sign, days, h, mi, s, micro
                ))
            }
        }
    }

    fn row_to_json(row: &Row) -> String {
        let mut map = Map::new();
        for i in 0..row.len() {
            let name = row.columns_ref()[i].name_str().to_string();
            let value = row
                .as_ref(i)
                .map(Self::mysql_value_to_json)
                .unwrap_or(Value::Null);
            map.insert(name, value);
        }
        serde_json::to_string(&Value::Object(map)).unwrap_or_default()
    }

    pub async fn sync(
        &mut self,
        offsets: Arc<Offsets>,
        shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
    ) {
        info!("MySQL input plugin starting sync");

        let (_pool, mut conn) = match Self::connect_pool(&self.config).await {
            Ok(c) => c,
            Err(e) => {
                error!("Failed to connect to MySQL: {}", e);
                return;
            }
        };

        let db_name = Self::get_database_name(&mut conn).await;

        let tables = match &self.config.tables {
            Some(t) => t.clone(),
            None => match Self::discover_tables(&mut conn).await {
                Ok(t) => {
                    info!("Discovered {} tables from MySQL", t.len());
                    t
                }
                Err(e) => {
                    error!("Failed to discover MySQL tables: {}", e);
                    return;
                }
            },
        };

        const DEFAULT_BATCH_ROWS: usize = 10_000;
        let batch_size = DEFAULT_BATCH_ROWS;

        for table_fq in &tables {
            let parts: Vec<&str> = table_fq.splitn(2, '.').collect();
            let (schema, table) = if parts.len() == 2 {
                (parts[0], parts[1])
            } else {
                (db_name.as_str(), parts[0])
            };

            let namespace = format!("mysql.{}.{}.{}", db_name, schema, table);
            let offset_key = OffsetKey {
                namespace: format!("mysql:{}.{}.{}", db_name, schema, table),
                partition: table_fq.clone(),
            };

            if offsets.validate(&offset_key, OffsetTypes::Closed, 1) == Some(true) {
                info!("Skipping already-ingested table: {}", table_fq);
                continue;
            }

            info!("Ingesting table: {} -> namespace: {}", table_fq, namespace);

            let q_schema = Self::escape_ident(schema);
            let q_table = Self::escape_ident(table);
            let query_sql = format!("SELECT * FROM {}.{}", q_schema, q_table);

            let rows: Vec<Row> = match conn.query(query_sql.as_str()).await {
                Ok(r) => r,
                Err(e) => {
                    error!("Failed to query table {}: {}", table_fq, e);
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
                    source_uri: format!("mysql://{}/{}", db_name, table_fq),
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

        info!("MySQL input plugin sync complete");
    }
}

#[async_trait]
impl DataSource for DataSourceMysqlPlugin {
    async fn sync(
        &mut self,
        offsets: Arc<Offsets>,
        output: Arc<Box<dyn DataSink + Send + Sync>>,
    ) -> Result<(), std::io::Error> {
        self.sync(offsets, output).await;
        Ok(())
    }
}
