use std::sync::Arc;

use async_trait::async_trait;
use serde_derive::Deserialize;
use tokio_postgres::{NoTls, Row};
use tracing::info;

use crate::helpers::configuration::{Config, DataSourcePluginConfig};
use crate::helpers::offsets::{OffsetKey, Offsets};
use crate::ingest_work::{Ingest, IngestBatch, IngestTask, IngestTasks};
use crate::plugins::{DataSink, DataSource};

#[derive(Debug, Deserialize, Clone)]
pub struct DataSourcePostgresPluginConfig {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub password: Option<String>,
    pub database: Option<String>,
    pub connection_string: Option<String>,
    pub tables: Option<Vec<String>>,
    pub query: Option<String>,
    pub batch_size_rows: Option<usize>,
    pub format: Option<String>,
    pub batch_size_bytes: Option<i64>,
    pub batch_size_seconds: Option<i64>,
}

impl From<DataSourcePluginConfig> for DataSourcePostgresPluginConfig {
    fn from(plugin_config: DataSourcePluginConfig) -> Self {
        match plugin_config {
            DataSourcePluginConfig::Postgres(config) => config,
            _ => panic!("Invalid plugin type for Postgres input"),
        }
    }
}

pub struct DataSourcePostgresPlugin {
    ingest: Ingest,
    config: DataSourcePostgresPluginConfig,
}

impl DataSourcePostgresPlugin {
    pub async fn new() -> Self {
        let config: DataSourcePostgresPluginConfig =
            match Config::get_pipeline_input_plugin_config() {
                Ok(c) => c.into(),
                Err(_) => DataSourcePostgresPluginConfig {
                    host: Some(Config::getenv("POSTGRES_HOST", "localhost")),
                    port: Some(5432),
                    user: Some(Config::getenv("POSTGRES_USER", "postgres")),
                    password: Some(Config::getenv("POSTGRES_PASSWORD", "")),
                    database: Some(Config::getenv("POSTGRES_DATABASE", "")),
                    connection_string: None,
                    tables: None,
                    query: None,
                    batch_size_rows: None,
                    format: None,
                    batch_size_bytes: None,
                    batch_size_seconds: None,
                },
            };
        Self {
            ingest: Ingest::new(),
            config,
        }
    }

    fn connection_string(&self) -> String {
        if let Some(ref cs) = self.config.connection_string {
            return cs.clone();
        }
        format!(
            "host={} port={} user={} password={} dbname={}",
            self.config.host.as_deref().unwrap_or("localhost"),
            self.config.port.unwrap_or(5432),
            self.config.user.as_deref().unwrap_or("postgres"),
            self.config.password.as_deref().unwrap_or(""),
            self.config.database.as_deref().unwrap_or(""),
        )
    }

    fn row_to_json(row: &Row) -> String {
        let mut map = serde_json::Map::new();
        for (i, col) in row.columns().iter().enumerate() {
            let val: serde_json::Value = if let Ok(v) = row.try_get::<_, String>(i) {
                serde_json::Value::String(v)
            } else if let Ok(v) = row.try_get::<_, i64>(i) {
                serde_json::Value::Number(v.into())
            } else if let Ok(v) = row.try_get::<_, i32>(i) {
                serde_json::Value::Number(v.into())
            } else if let Ok(v) = row.try_get::<_, f64>(i) {
                serde_json::json!(v)
            } else if let Ok(v) = row.try_get::<_, bool>(i) {
                serde_json::Value::Bool(v)
            } else if let Ok(v) = row.try_get::<_, Option<String>>(i) {
                match v {
                    Some(s) => serde_json::Value::String(s),
                    None => serde_json::Value::Null,
                }
            } else {
                serde_json::Value::Null
            };
            map.insert(col.name().to_string(), val);
        }
        serde_json::to_string(&map).unwrap_or_else(|_| "{}".to_string())
    }
}

#[async_trait]
impl DataSource for DataSourcePostgresPlugin {
    async fn sync(
        &mut self,
        offsets: Arc<Offsets>,
        shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
    ) -> Result<(), std::io::Error> {
        let conn_str = self.connection_string();
        let (client, connection) = tokio_postgres::connect(&conn_str, NoTls)
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        tokio::spawn(async move {
            if let Err(e) = connection.await {
                tracing::error!("Postgres connection error: {}", e);
            }
        });

        let queries: Vec<(String, String)> = if let Some(ref q) = self.config.query {
            vec![("query".to_string(), q.clone())]
        } else if let Some(ref tables) = self.config.tables {
            tables
                .iter()
                .map(|t| (t.clone(), format!("SELECT * FROM {}", t)))
                .collect()
        } else {
            let rows = client
                .query(
                    "SELECT tablename FROM pg_tables WHERE schemaname = 'public'",
                    &[],
                )
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            rows.iter()
                .map(|r| {
                    let name: String = r.get(0);
                    let q = format!("SELECT * FROM {}", name);
                    (name, q)
                })
                .collect()
        };

        let batch_size = self.config.batch_size_rows.unwrap_or(10_000);

        for (table_name, query) in queries {
            let namespace = format!("postgres.{}", table_name);
            info!("Postgres input: querying {}", table_name);

            let rows = client
                .query(&query, &[])
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?;

            let offset_key = OffsetKey {
                namespace: namespace.clone(),
                partition: table_name.clone(),
            };

            let mut current_batch: Vec<IngestBatch> = Vec::new();

            for row in &rows {
                let json_str = Self::row_to_json(row);
                let bytes = json_str.len();
                current_batch.push(IngestBatch {
                    offset_key: offset_key.clone(),
                    data: json_str,
                    bytes,
                    source_uri: format!("postgres://{}", table_name),
                    namespace: Some(namespace.clone()),
                });

                if current_batch.len() >= batch_size {
                    let mut ingest_tasks = IngestTasks::new();
                    ingest_tasks.add(IngestTask::new(
                        std::mem::take(&mut current_batch),
                        offsets.clone(),
                        shared_output.clone(),
                    ));
                    self.ingest.ingest_file(
                        &Arc::new(ingest_tasks),
                        &offsets,
                        shared_output.clone(),
                    );
                }
            }

            if !current_batch.is_empty() {
                let mut ingest_tasks = IngestTasks::new();
                ingest_tasks.add(IngestTask::new(
                    current_batch,
                    offsets.clone(),
                    shared_output.clone(),
                ));
                self.ingest.ingest_file(
                    &Arc::new(ingest_tasks),
                    &offsets,
                    shared_output.clone(),
                );
            }
        }

        Ok(())
    }
}
