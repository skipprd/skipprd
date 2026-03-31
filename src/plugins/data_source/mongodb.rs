use std::sync::Arc;

use async_trait::async_trait;
use futures::TryStreamExt;
use mongodb::{bson::Document, options::ClientOptions, Client};
use serde_derive::Deserialize;
use tracing::info;

use crate::helpers::configuration::{Config, DataSourcePluginConfig};
use crate::helpers::offsets::{OffsetKey, Offsets};
use crate::ingest_work::{Ingest, IngestBatch, IngestTask, IngestTasks};
use crate::plugins::{DataSink, DataSource};

#[derive(Debug, Deserialize, Clone)]
pub struct DataSourceMongodbPluginConfig {
    pub connection_string: String,
    pub database: String,
    pub collection: String,
    pub filter: Option<String>,
    pub batch_size_rows: Option<usize>,
    pub format: Option<String>,
    pub batch_size_bytes: Option<i64>,
    pub batch_size_seconds: Option<i64>,
}

impl From<DataSourcePluginConfig> for DataSourceMongodbPluginConfig {
    fn from(plugin_config: DataSourcePluginConfig) -> Self {
        match plugin_config {
            DataSourcePluginConfig::Mongodb(config) => config,
            _ => panic!("Invalid plugin type for Mongodb"),
        }
    }
}

pub struct DataSourceMongodbPlugin {
    ingest: Ingest,
    config: DataSourceMongodbPluginConfig,
}

impl DataSourceMongodbPlugin {
    pub async fn new() -> Self {
        let config: DataSourceMongodbPluginConfig =
            match Config::get_pipeline_input_plugin_config() {
                Ok(c) => c.into(),
                Err(_) => DataSourceMongodbPluginConfig {
                    connection_string: Config::getenv("MONGODB_CONNECTION_STRING", ""),
                    database: Config::getenv("MONGODB_DATABASE", ""),
                    collection: Config::getenv("MONGODB_COLLECTION", ""),
                    filter: None,
                    batch_size_rows: None,
                    format: None,
                    batch_size_bytes: Some(
                        Config::getenv("DATA_SOURCE_BATCH_SIZE_BYTES", "1024000")
                            .parse()
                            .unwrap_or(1_024_000),
                    ),
                    batch_size_seconds: Some(
                        Config::getenv("DATA_SOURCE_BATCH_SIZE_SECONDS", "600")
                            .parse()
                            .unwrap_or(600),
                    ),
                },
            };
        Self {
            ingest: Ingest::new(),
            config,
        }
    }
}

#[async_trait]
impl DataSource for DataSourceMongodbPlugin {
    async fn sync(
        &mut self,
        offsets: Arc<Offsets>,
        shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
    ) -> Result<(), std::io::Error> {
        let client_options = ClientOptions::parse(&self.config.connection_string)
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let client =
            Client::with_options(client_options).map_err(|e| std::io::Error::other(e.to_string()))?;
        let db = client.database(&self.config.database);
        let collection = db.collection::<Document>(&self.config.collection);

        let filter_doc: Option<Document> = match &self.config.filter {
            Some(f) if !f.is_empty() => {
                let v: serde_json::Value =
                    serde_json::from_str(f).map_err(|e| std::io::Error::other(e.to_string()))?;
                let bson = mongodb::bson::to_bson(&v)
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                Some(
                    bson.as_document()
                        .cloned()
                        .ok_or_else(|| std::io::Error::other("filter must be a JSON object"))?,
                )
            }
            _ => None,
        };

        let namespace = format!(
            "mongodb.{}.{}",
            self.config.database, self.config.collection
        );
        let offset_key = OffsetKey {
            namespace: namespace.clone(),
            partition: String::new(),
        };

        info!(
            "MongoDB: reading {}.{}",
            self.config.database, self.config.collection
        );

        let mut cursor = collection
            .find(filter_doc.unwrap_or_default())
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let chunk_bytes = self.config.batch_size_bytes.unwrap_or(1_024_000) as usize;
        let mut current_batch: Vec<IngestBatch> = Vec::new();
        let mut current_bytes: usize = 0;
        let source_uri = format!(
            "mongodb://{}/{}",
            self.config.database, self.config.collection
        );

        while let Some(doc) = cursor
            .try_next()
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?
        {
            let json_str =
                serde_json::to_string(&doc).map_err(|e| std::io::Error::other(e.to_string()))?;
            let bytes = json_str.len();

            current_batch.push(IngestBatch {
                offset_key: offset_key.clone(),
                data: json_str,
                bytes,
                source_uri: source_uri.clone(),
                namespace: Some(namespace.clone()),
            });
            current_bytes += bytes;

            if current_bytes >= chunk_bytes {
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
                current_bytes = 0;
            }
        }

        if !current_batch.is_empty() {
            let mut ingest_tasks = IngestTasks::new();
            ingest_tasks.add(IngestTask::new(
                current_batch,
                offsets.clone(),
                shared_output.clone(),
            ));
            self.ingest
                .ingest_file(&Arc::new(ingest_tasks), &offsets, shared_output.clone());
        }

        Ok(())
    }
}
