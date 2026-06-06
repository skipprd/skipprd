use std::sync::Arc;

use async_trait::async_trait;
use reqwest::Client;
use serde_derive::Deserialize;
use tracing::info;

use crate::helpers::configuration::Config;
use crate::helpers::plugin_config::PluginConfigEntry;
use skippr_runtime_sdk::plugins::DataSource;
use skippr_runtime_sdk::progress::OffsetKey;
use skippr_runtime_sdk::source_compat::{submit_payload_batches, IngestBatch, SourceSyncContext};

#[derive(Debug, Deserialize, Clone)]
pub struct DataSourceClickhousePluginConfig {
    pub url: String,
    pub database: Option<String>,
    pub user: Option<String>,
    pub password: Option<String>,
    pub tables: Option<Vec<String>>,
    pub query: Option<String>,
    pub batch_size_rows: Option<usize>,
    pub format: Option<String>,
    pub batch_size_bytes: Option<i64>,
    pub batch_size_seconds: Option<i64>,
}

impl TryFrom<PluginConfigEntry> for DataSourceClickhousePluginConfig {
    type Error = String;

    fn try_from(plugin_config: PluginConfigEntry) -> Result<Self, Self::Error> {
        plugin_config.decode_for_plugin("Clickhouse")
    }
}

pub struct DataSourceClickhousePlugin {
    config: DataSourceClickhousePluginConfig,
    client: Client,
}

impl DataSourceClickhousePlugin {
    pub async fn new() -> Self {
        let config: DataSourceClickhousePluginConfig =
            match Config::get_pipeline_input_plugin_config() {
                Ok(c) => c.try_into().unwrap_or_else(|e| panic!("{}", e)),
                Err(_) => DataSourceClickhousePluginConfig {
                    url: Config::getenv("CLICKHOUSE_URL", "http://localhost:8123"),
                    database: Some(Config::getenv("CLICKHOUSE_DATABASE", "default")),
                    user: Some(Config::getenv("CLICKHOUSE_USER", "default")),
                    password: None,
                    tables: None,
                    query: None,
                    batch_size_rows: None,
                    format: None,
                    batch_size_bytes: None,
                    batch_size_seconds: None,
                },
            };
        Self {
            config,
            client: Client::new(),
        }
    }

    pub fn with_runtime_config(config: DataSourceClickhousePluginConfig) -> Self {
        Self {
            config,
            client: Client::new(),
        }
    }

    async fn query_json_rows(&self, sql: &str) -> Result<Vec<String>, std::io::Error> {
        let full_query = format!("{} FORMAT JSONEachRow", sql);
        let mut req = self.client.post(&self.config.url).body(full_query);
        if let Some(ref user) = self.config.user {
            req = req.header("X-ClickHouse-User", user.as_str());
        }
        if let Some(ref password) = self.config.password {
            req = req.header("X-ClickHouse-Key", password.as_str());
        }
        if let Some(ref db) = self.config.database {
            req = req.header("X-ClickHouse-Database", db.as_str());
        }

        let resp = req
            .send()
            .await
            .map_err(|e| std::io::Error::other(format!("ClickHouse request failed: {}", e)))?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(std::io::Error::other(format!(
                "ClickHouse query error: {}",
                body
            )));
        }

        let body = resp
            .text()
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        Ok(body
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.to_string())
            .collect())
    }
}

#[async_trait]
impl DataSource for DataSourceClickhousePlugin {
    async fn sync(&mut self, ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error> {
        let queries: Vec<(String, String)> = if let Some(ref q) = self.config.query {
            vec![("query".to_string(), q.clone())]
        } else if let Some(ref tables) = self.config.tables {
            tables
                .iter()
                .map(|t| (t.clone(), format!("SELECT * FROM {}", t)))
                .collect()
        } else {
            return Err(std::io::Error::other(
                "ClickHouse: must specify either 'tables' or 'query'",
            ));
        };

        let batch_size = self.config.batch_size_rows.unwrap_or(10_000);
        let db = self.config.database.as_deref().unwrap_or("default");

        for (table_name, query) in queries {
            let namespace = format!("clickhouse.{}.{}", db, table_name);
            info!("ClickHouse input: querying {}", table_name);

            let rows = self.query_json_rows(&query).await?;

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
                    offset_pos: None,
                    source_uri: format!("clickhouse://{}/{}", db, table_name),
                    namespace: Some(namespace.clone()),
                    cdc_rows: None,
                });

                if current_batch.len() >= batch_size {
                    submit_payload_batches(ctx.as_ref(), std::mem::take(&mut current_batch))?;
                }
            }

            if !current_batch.is_empty() {
                submit_payload_batches(ctx.as_ref(), current_batch)?;
            }
        }

        Ok(())
    }

    fn execution_contract(&self) -> skippr_runtime_sdk::plugins::SourceExecutionContract {
        skippr_runtime_sdk::plugins::SourceExecutionContract::finite()
    }
}
