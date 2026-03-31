use std::collections::HashMap;

use crate::helpers::configuration::DataSinkPluginConfig;
use crate::plugins::parquet_util::serialize_to_parquet;
use crate::plugins::DataSink;
use async_trait::async_trait;
use datafusion::execution::SendableRecordBatchStream;
use reqwest::Client;
use serde_derive::Deserialize;
use tracing::info;

#[derive(Debug, Deserialize, Clone)]
pub struct DataSinkDatabricksPluginConfig {
    #[serde(default)]
    pub workspace_url: String,
    #[serde(default)]
    pub token: String,
    pub warehouse_id: Option<String>,
    pub catalog: Option<String>,
    pub schema: Option<String>,
    pub table: Option<String>,
    pub format: Option<String>,
    pub delta_table_uri: Option<String>,
    #[serde(default)]
    pub storage_options: Option<HashMap<String, String>>,
}

impl From<DataSinkPluginConfig> for DataSinkDatabricksPluginConfig {
    fn from(plugin_config: DataSinkPluginConfig) -> Self {
        match plugin_config {
            DataSinkPluginConfig::Databricks(config) => config,
            _ => panic!("Invalid plugin type for Databricks"),
        }
    }
}

pub struct DataSinkDatabricksPlugin {
    client: Client,
    config: DataSinkDatabricksPluginConfig,
}

#[async_trait]
impl DataSink for DataSinkDatabricksPlugin {
    async fn sync(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
    ) -> Result<(), std::io::Error> {
        if self.config.delta_table_uri.is_some() {
            self.sync_delta(stream, filename).await
        } else {
            self.sync_copy(stream, filename).await
        }
    }
}

impl DataSinkDatabricksPlugin {
    pub async fn new_with_config(
        _buffer_name: String,
        config: DataSinkDatabricksPluginConfig,
    ) -> Self {
        Self {
            client: Client::new(),
            config,
        }
    }

    async fn sync_copy(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
    ) -> Result<(), std::io::Error> {
        use crate::metrics::counters;
        counters::inc_uploads_in_flight();

        let parquet_bytes = serialize_to_parquet(stream).await?;

        let upload_path = format!(
            "/Volumes/{}/{}/staging/{}",
            self.config.catalog.as_deref().unwrap_or("main"),
            self.config.schema.as_deref().unwrap_or("default"),
            filename,
        );

        let url = format!(
            "{}/api/2.0/fs/files{}",
            self.config.workspace_url.trim_end_matches('/'),
            upload_path,
        );

        self.client
            .put(&url)
            .bearer_auth(&self.config.token)
            .header("Content-Type", "application/octet-stream")
            .body(parquet_bytes.bytes.to_vec())
            .send()
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?
            .error_for_status()
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        if let Some(ref warehouse_id) = self.config.warehouse_id {
            let table = self.config.table.as_deref().unwrap_or("data");
            let sql = format!(
                "COPY INTO {}.{}.{} FROM '{}' FILEFORMAT = PARQUET",
                self.config.catalog.as_deref().unwrap_or("main"),
                self.config.schema.as_deref().unwrap_or("default"),
                table,
                upload_path,
            );

            let stmt_url = format!(
                "{}/api/2.0/sql/statements",
                self.config.workspace_url.trim_end_matches('/'),
            );

            self.client
                .post(&stmt_url)
                .bearer_auth(&self.config.token)
                .json(&serde_json::json!({
                    "warehouse_id": warehouse_id,
                    "statement": sql,
                    "wait_timeout": "30s",
                }))
                .send()
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?
                .error_for_status()
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        }

        info!("Databricks: uploaded {}", upload_path);
        counters::dec_uploads_in_flight();
        Ok(())
    }

    async fn sync_delta(
        &self,
        stream: SendableRecordBatchStream,
        _filename: String,
    ) -> Result<(), std::io::Error> {
        use crate::metrics::counters;
        counters::inc_uploads_in_flight();

        let delta_uri = self.config.delta_table_uri.as_deref().unwrap();
        let storage_opts = self.config.storage_options.clone().unwrap_or_default();

        let parquet_bytes = serialize_to_parquet(stream).await.map_err(|e| {
            counters::dec_uploads_in_flight();
            e
        })?;
        let total_rows = parquet_bytes.meta_data.num_rows as u64;

        if total_rows == 0 {
            counters::dec_uploads_in_flight();
            return Ok(());
        }

        let tmp = tempfile::NamedTempFile::new().map_err(|e| {
            counters::dec_uploads_in_flight();
            std::io::Error::other(format!("temp file: {}", e))
        })?;
        std::fs::write(tmp.path(), &parquet_bytes.bytes).map_err(|e| {
            counters::dec_uploads_in_flight();
            std::io::Error::other(format!("temp write: {}", e))
        })?;

        let ctx = deltalake::datafusion::prelude::SessionContext::new();
        let df = ctx
            .read_parquet(
                tmp.path().to_str().unwrap(),
                deltalake::datafusion::prelude::ParquetReadOptions::default(),
            )
            .await
            .map_err(|e| {
                counters::dec_uploads_in_flight();
                std::io::Error::other(format!("Delta read parquet: {}", e))
            })?;

        let delta_batches: Vec<deltalake::arrow::record_batch::RecordBatch> =
            df.collect().await.map_err(|e| {
                counters::dec_uploads_in_flight();
                std::io::Error::other(format!("Delta collect: {}", e))
            })?;

        let table_result =
            deltalake::open_table_with_storage_options(delta_uri, storage_opts.clone()).await;

        match table_result {
            Ok(table) => {
                deltalake::DeltaOps(table)
                    .write(delta_batches)
                    .with_save_mode(deltalake::protocol::SaveMode::Append)
                    .await
                    .map_err(|e| std::io::Error::other(format!("Delta write: {}", e)))?;
            }
            Err(_) => {
                let ops = deltalake::DeltaOps::try_from_uri_with_storage_options(
                    delta_uri,
                    storage_opts,
                )
                .await
                .map_err(|e| std::io::Error::other(format!("Delta init: {}", e)))?;

                ops.write(delta_batches)
                    .with_save_mode(deltalake::protocol::SaveMode::Append)
                    .await
                    .map_err(|e| std::io::Error::other(format!("Delta write: {}", e)))?;
            }
        }

        counters::add_parquet_rows(total_rows);
        counters::add_upload(1);
        counters::dec_uploads_in_flight();
        info!("Delta Lake: wrote {} rows to {}", total_rows, delta_uri);
        Ok(())
    }
}
