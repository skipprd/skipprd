use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use serde_derive::Deserialize;
use tracing::info;

use crate::helpers::configuration::Config;
use crate::helpers::plugin_config::PluginConfigEntry;
use skippr_runtime_sdk::plugins::DataSource;
use skippr_runtime_sdk::progress::OffsetKey;
use skippr_runtime_sdk::source_compat::{submit_payload_batches, IngestBatch, SourceSyncContext};

#[derive(Debug, Deserialize, Clone)]
pub struct DataSourceDeltaLakePluginConfig {
    pub table_uri: String,
    #[serde(default)]
    pub storage_options: Option<HashMap<String, String>>,
    pub version: Option<i64>,
    pub filter: Option<String>,
    pub batch_size_rows: Option<usize>,
    pub format: Option<String>,
    pub batch_size_bytes: Option<i64>,
    pub batch_size_seconds: Option<i64>,
}

impl TryFrom<PluginConfigEntry> for DataSourceDeltaLakePluginConfig {
    type Error = String;

    fn try_from(plugin_config: PluginConfigEntry) -> Result<Self, Self::Error> {
        plugin_config.decode_for_plugin("DeltaLake")
    }
}

pub struct DataSourceDeltaLakePlugin {
    config: DataSourceDeltaLakePluginConfig,
}

impl DataSourceDeltaLakePlugin {
    pub async fn new() -> Self {
        let config: DataSourceDeltaLakePluginConfig =
            match Config::get_pipeline_input_plugin_config() {
                Ok(c) => c.try_into().unwrap_or_else(|e| panic!("{}", e)),
                Err(_) => DataSourceDeltaLakePluginConfig {
                    table_uri: Config::getenv("DELTA_TABLE_URI", ""),
                    storage_options: None,
                    version: None,
                    filter: None,
                    batch_size_rows: None,
                    format: None,
                    batch_size_bytes: None,
                    batch_size_seconds: None,
                },
            };
        Self { config }
    }

    pub fn with_runtime_config(config: DataSourceDeltaLakePluginConfig) -> Self {
        Self { config }
    }

    fn batch_to_json_rows(
        batch: &deltalake::arrow::record_batch::RecordBatch,
    ) -> Result<Vec<String>, std::io::Error> {
        let mut buf = Vec::new();
        {
            let mut writer = deltalake::arrow::json::LineDelimitedWriter::new(&mut buf);
            writer
                .write(batch)
                .map_err(|e| std::io::Error::other(format!("Arrow JSON write: {}", e)))?;
            writer
                .finish()
                .map_err(|e| std::io::Error::other(format!("Arrow JSON finish: {}", e)))?;
        }
        let text =
            String::from_utf8(buf).map_err(|e| std::io::Error::other(format!("UTF-8: {}", e)))?;
        Ok(text
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.to_string())
            .collect())
    }
}

#[async_trait]
impl DataSource for DataSourceDeltaLakePlugin {
    async fn sync(&mut self, sync_ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error> {
        let storage_options = self.config.storage_options.clone().unwrap_or_default();

        let table_url = deltalake::ensure_table_uri(&self.config.table_uri)
            .map_err(|e| std::io::Error::other(format!("Delta Lake URI: {}", e)))?;

        let table = if let Some(version) = self.config.version {
            deltalake::open_table_with_version(table_url, version as u64)
                .await
                .map_err(|e| std::io::Error::other(format!("Delta Lake open: {}", e)))?
        } else {
            deltalake::open_table_with_storage_options(table_url, storage_options)
                .await
                .map_err(|e| std::io::Error::other(format!("Delta Lake open: {}", e)))?
        };

        info!(
            "Delta Lake: opened table {} (version {:?})",
            self.config.table_uri,
            table.version()
        );

        let df_ctx = deltalake::datafusion::prelude::SessionContext::new();
        let provider = table
            .table_provider()
            .await
            .map_err(|e| std::io::Error::other(format!("Delta table provider: {}", e)))?;
        df_ctx
            .register_table("delta_source", provider)
            .map_err(|e| std::io::Error::other(format!("Delta register: {}", e)))?;

        let sql = if let Some(ref filter) = self.config.filter {
            format!("SELECT * FROM delta_source WHERE {}", filter)
        } else {
            "SELECT * FROM delta_source".to_string()
        };

        let df = df_ctx
            .sql(&sql)
            .await
            .map_err(|e| std::io::Error::other(format!("Delta query: {}", e)))?;

        let mut stream = df
            .execute_stream()
            .await
            .map_err(|e| std::io::Error::other(format!("Delta execute: {}", e)))?;

        let namespace = format!("delta_lake.{}", self.config.table_uri);
        let offset_key = OffsetKey {
            namespace: namespace.clone(),
            partition: "default".to_string(),
        };

        let batch_size = self.config.batch_size_rows.unwrap_or(10_000);
        let mut current_batch: Vec<IngestBatch> = Vec::new();

        while let Some(batch_result) = stream.next().await {
            let batch =
                batch_result.map_err(|e| std::io::Error::other(format!("Delta stream: {}", e)))?;

            let json_rows = Self::batch_to_json_rows(&batch)?;

            for json_str in json_rows {
                let bytes = json_str.len();
                current_batch.push(IngestBatch {
                    offset_key: offset_key.clone(),
                    data: json_str,
                    bytes,
                    offset_pos: None,
                    source_uri: format!("delta://{}", self.config.table_uri),
                    namespace: Some(namespace.clone()),
                    cdc_rows: None,
                });

                if current_batch.len() >= batch_size {
                    submit_payload_batches(sync_ctx.as_ref(), std::mem::take(&mut current_batch))?;
                }
            }
        }

        if !current_batch.is_empty() {
            submit_payload_batches(sync_ctx.as_ref(), current_batch)?;
        }

        info!("Delta Lake: sync complete for {}", self.config.table_uri);
        Ok(())
    }

    fn execution_contract(&self) -> skippr_runtime_sdk::plugins::SourceExecutionContract {
        skippr_runtime_sdk::plugins::SourceExecutionContract::finite()
    }
}
