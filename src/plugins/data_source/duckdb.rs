use std::sync::Arc;

use async_trait::async_trait;
use serde_derive::Deserialize;
use tracing::info;

use crate::helpers::configuration::{Config, DataSourcePluginConfig};
use crate::helpers::offsets::{OffsetKey, Offsets};
use crate::ingest_work::{Ingest, IngestBatch, IngestTask, IngestTasks};
use crate::plugins::{DataSink, DataSource};

#[derive(Debug, Deserialize, Clone)]
pub struct DataSourceDuckdbPluginConfig {
    pub connection_string: String,
    pub motherduck_token: Option<String>,
    pub database: Option<String>,
    pub tables: Option<Vec<String>>,
    pub query: Option<String>,
    pub batch_size_rows: Option<usize>,
    pub format: Option<String>,
    pub batch_size_bytes: Option<i64>,
    pub batch_size_seconds: Option<i64>,
}

impl From<DataSourcePluginConfig> for DataSourceDuckdbPluginConfig {
    fn from(plugin_config: DataSourcePluginConfig) -> Self {
        match plugin_config {
            DataSourcePluginConfig::Duckdb(config) => config,
            _ => panic!("Invalid plugin type for DuckDB input"),
        }
    }
}

pub struct DataSourceDuckdbPlugin {
    ingest: Ingest,
    config: DataSourceDuckdbPluginConfig,
}

impl DataSourceDuckdbPlugin {
    pub async fn new() -> Self {
        let config: DataSourceDuckdbPluginConfig =
            match Config::get_pipeline_input_plugin_config() {
                Ok(c) => c.into(),
                Err(_) => DataSourceDuckdbPluginConfig {
                    connection_string: Config::getenv("DUCKDB_CONNECTION_STRING", ":memory:"),
                    motherduck_token: {
                        let v = Config::getenv("MOTHERDUCK_TOKEN", "");
                        if v.is_empty() { None } else { Some(v) }
                    },
                    database: None,
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

    fn query_rows(
        conn: &duckdb::Connection,
        sql: &str,
    ) -> Result<Vec<String>, std::io::Error> {
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| std::io::Error::other(format!("DuckDB prepare: {}", e)))?;

        let column_count = stmt.column_count();
        let column_names: Vec<String> = (0..column_count)
            .map(|i| stmt.column_name(i).map_or("col".to_string(), |v| v.to_string()))
            .collect();

        let rows = stmt
            .query_map([], |row| {
                let mut map = serde_json::Map::new();
                for (i, name) in column_names.iter().enumerate() {
                    let val: serde_json::Value =
                        if let Ok(v) = row.get::<_, String>(i) {
                            serde_json::Value::String(v)
                        } else if let Ok(v) = row.get::<_, i64>(i) {
                            serde_json::Value::Number(v.into())
                        } else if let Ok(v) = row.get::<_, i32>(i) {
                            serde_json::Value::Number(v.into())
                        } else if let Ok(v) = row.get::<_, f64>(i) {
                            serde_json::json!(v)
                        } else if let Ok(v) = row.get::<_, bool>(i) {
                            serde_json::Value::Bool(v)
                        } else if let Ok(v) = row.get::<_, Option<String>>(i) {
                            match v {
                                Some(s) => serde_json::Value::String(s),
                                None => serde_json::Value::Null,
                            }
                        } else {
                            serde_json::Value::Null
                        };
                    map.insert(name.clone(), val);
                }
                Ok(serde_json::to_string(&map).unwrap_or_else(|_| "{}".to_string()))
            })
            .map_err(|e| std::io::Error::other(format!("DuckDB query: {}", e)))?;

        let mut results = Vec::new();
        for row_result in rows {
            results.push(
                row_result.map_err(|e| std::io::Error::other(format!("DuckDB row: {}", e)))?,
            );
        }
        Ok(results)
    }
}

#[async_trait]
impl DataSource for DataSourceDuckdbPlugin {
    async fn sync(
        &mut self,
        offsets: Arc<Offsets>,
        shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
    ) -> Result<(), std::io::Error> {
        let config = self.config.clone();

        let queries: Vec<(String, String)> = if let Some(ref q) = config.query {
            vec![("query".to_string(), q.clone())]
        } else if let Some(ref tables) = config.tables {
            tables
                .iter()
                .map(|t| (t.clone(), format!("SELECT * FROM {}", t)))
                .collect()
        } else {
            return Err(std::io::Error::other(
                "DuckDB: must specify either 'tables' or 'query'",
            ));
        };

        let all_rows = tokio::task::spawn_blocking(move || {
            let conn = duckdb::Connection::open(&config.connection_string)
                .map_err(|e| std::io::Error::other(format!("DuckDB open: {}", e)))?;

            if let Some(ref token) = config.motherduck_token {
                conn.execute_batch(&format!("SET motherduck_token='{}'", token))
                    .map_err(|e| std::io::Error::other(format!("DuckDB SET token: {}", e)))?;
            }

            if let Some(ref db) = config.database {
                conn.execute_batch(&format!("USE {}", db))
                    .map_err(|e| std::io::Error::other(format!("DuckDB USE: {}", e)))?;
            }

            let mut table_rows: Vec<(String, Vec<String>)> = Vec::new();
            for (table_name, query) in &queries {
                let rows = Self::query_rows(&conn, query)?;
                table_rows.push((table_name.clone(), rows));
            }

            Ok::<_, std::io::Error>(table_rows)
        })
        .await
        .map_err(|e| std::io::Error::other(format!("DuckDB spawn: {}", e)))??;

        let batch_size = self.config.batch_size_rows.unwrap_or(10_000);
        let db_label = self.config.database.as_deref().unwrap_or("duckdb");

        for (table_name, rows) in all_rows {
            let namespace = format!("duckdb.{}.{}", db_label, table_name);
            info!("DuckDB input: {} rows from {}", rows.len(), table_name);

            let offset_key = OffsetKey {
                namespace: namespace.clone(),
                partition: table_name.clone(),
            };

            let mut current_batch: Vec<IngestBatch> = Vec::new();

            for json_str in rows {
                let bytes = json_str.len();
                current_batch.push(IngestBatch {
                    offset_key: offset_key.clone(),
                    data: json_str,
                    bytes,
                    source_uri: format!("duckdb://{}/{}", db_label, table_name),
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
