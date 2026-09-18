use skippr_runtime_sdk::SkipprConfig;
use std::sync::Arc;

use async_trait::async_trait;
use reqwest::Client;
use serde_derive::Deserialize;
use tracing::info;

use crate::helpers::plugin_config::PluginConfigEntry;
use skippr_runtime_sdk::plugins::DataSource;
use skippr_runtime_sdk::progress::OffsetKey;
use skippr_runtime_sdk::source_compat::{submit_payload_batches, IngestBatch, SourceSyncContext};

#[derive(Debug, Deserialize, SkipprConfig, Clone)]
pub struct DataSourceMotherduckPluginConfig {
    #[skippr(secret)]
    pub motherduck_token: String,
    pub database: Option<String>,
    pub tables: Option<Vec<String>>,
    pub query: Option<String>,
    pub batch_size_rows: Option<usize>,
    pub format: Option<String>,
    pub batch_size_bytes: Option<i64>,
    pub batch_size_seconds: Option<i64>,
}

impl TryFrom<PluginConfigEntry> for DataSourceMotherduckPluginConfig {
    type Error = String;

    fn try_from(plugin_config: PluginConfigEntry) -> Result<Self, Self::Error> {
        plugin_config.decode_for_plugin("Motherduck")
    }
}

pub struct DataSourceMotherduckPlugin {
    config: DataSourceMotherduckPluginConfig,
    client: Client,
}

const MOTHERDUCK_SQL_ENDPOINT: &str = "https://api.motherduck.com/v1/sql";

impl DataSourceMotherduckPlugin {

    pub fn with_runtime_config(config: DataSourceMotherduckPluginConfig) -> Self {
        Self {
            config,
            client: Client::new(),
        }
    }

    async fn execute_sql(&self, sql: &str) -> Result<serde_json::Value, std::io::Error> {
        let mut payload = serde_json::json!({ "sql": sql });
        if let Some(ref db) = self.config.database {
            payload["database"] = serde_json::json!(db);
        }

        let resp = self
            .client
            .post(MOTHERDUCK_SQL_ENDPOINT)
            .header(
                "Authorization",
                format!("Bearer {}", self.config.motherduck_token),
            )
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await
            .map_err(|e| std::io::Error::other(format!("MotherDuck request: {}", e)))?;

        let status = resp.status();
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| std::io::Error::other(format!("MotherDuck response parse: {}", e)))?;

        if !status.is_success() {
            let msg = body["message"]
                .as_str()
                .or_else(|| body["error"].as_str())
                .unwrap_or("unknown error");
            return Err(std::io::Error::other(format!(
                "MotherDuck API HTTP {}: {}",
                status, msg
            )));
        }

        Ok(body)
    }

    fn rows_from_response(body: &serde_json::Value) -> Vec<String> {
        let Some(data) = body.get("data").and_then(|d| d.as_array()) else {
            return Vec::new();
        };
        data.iter()
            .filter_map(|row| serde_json::to_string(row).ok())
            .collect()
    }
}

#[async_trait]
impl DataSource for DataSourceMotherduckPlugin {
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
                "MotherDuck: must specify either 'tables' or 'query'",
            ));
        };

        let batch_size = self.config.batch_size_rows.unwrap_or(10_000);
        let db_label = self.config.database.as_deref().unwrap_or("motherduck");

        for (table_name, query) in &queries {
            let body = self.execute_sql(query).await?;
            let rows = Self::rows_from_response(&body);

            let namespace = format!("motherduck.{}.{}", db_label, table_name);
            info!("MotherDuck input: {} rows from {}", rows.len(), table_name);

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
                    source_uri: format!("motherduck://{}/{}", db_label, table_name),
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
