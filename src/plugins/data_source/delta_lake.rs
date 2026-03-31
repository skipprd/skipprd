use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use serde_derive::Deserialize;
use tracing::info;

use crate::helpers::configuration::{Config, DataSourcePluginConfig};
use crate::helpers::offsets::{OffsetKey, Offsets};
use crate::ingest_work::{Ingest, IngestBatch, IngestTask, IngestTasks};
use crate::plugins::{DataSink, DataSource};

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

impl From<DataSourcePluginConfig> for DataSourceDeltaLakePluginConfig {
    fn from(plugin_config: DataSourcePluginConfig) -> Self {
        match plugin_config {
            DataSourcePluginConfig::DeltaLake(config) => config,
            _ => panic!("Invalid plugin type for Delta Lake input"),
        }
    }
}

pub struct DataSourceDeltaLakePlugin {
    ingest: Ingest,
    config: DataSourceDeltaLakePluginConfig,
}

impl DataSourceDeltaLakePlugin {
    pub async fn new() -> Self {
        let config: DataSourceDeltaLakePluginConfig =
            match Config::get_pipeline_input_plugin_config() {
                Ok(c) => c.into(),
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
        Self {
            ingest: Ingest::new(),
            config,
        }
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
        let text = String::from_utf8(buf)
            .map_err(|e| std::io::Error::other(format!("UTF-8: {}", e)))?;
        Ok(text
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.to_string())
            .collect())
    }
}

#[async_trait]
impl DataSource for DataSourceDeltaLakePlugin {
    async fn sync(
        &mut self,
        offsets: Arc<Offsets>,
        shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
    ) -> Result<(), std::io::Error> {
        let storage_options = self.config.storage_options.clone().unwrap_or_default();

        let table = if let Some(version) = self.config.version {
            deltalake::open_table_with_version(&self.config.table_uri, version)
                .await
                .map_err(|e| std::io::Error::other(format!("Delta Lake open: {}", e)))?
        } else {
            deltalake::open_table_with_storage_options(&self.config.table_uri, storage_options)
                .await
                .map_err(|e| std::io::Error::other(format!("Delta Lake open: {}", e)))?
        };

        info!(
            "Delta Lake: opened table {} (version {:?})",
            self.config.table_uri,
            table.version()
        );

        let ctx = deltalake::datafusion::prelude::SessionContext::new();
        ctx.register_table("delta_source", Arc::new(table))
            .map_err(|e| std::io::Error::other(format!("Delta register: {}", e)))?;

        let sql = if let Some(ref filter) = self.config.filter {
            format!("SELECT * FROM delta_source WHERE {}", filter)
        } else {
            "SELECT * FROM delta_source".to_string()
        };

        let df = ctx
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
            let batch = batch_result
                .map_err(|e| std::io::Error::other(format!("Delta stream: {}", e)))?;

            let json_rows = Self::batch_to_json_rows(&batch)?;

            for json_str in json_rows {
                let bytes = json_str.len();
                current_batch.push(IngestBatch {
                    offset_key: offset_key.clone(),
                    data: json_str,
                    bytes,
                    source_uri: format!("delta://{}", self.config.table_uri),
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

        info!("Delta Lake: sync complete for {}", self.config.table_uri);
        Ok(())
    }
}
