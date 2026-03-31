use crate::buffer::BufferChunker;
use crate::helpers::configuration::{Config, DataSinkPluginConfig};
use crate::ingest::partition_time::TimePartitioner;
use crate::plugins::parquet_util::serialize_to_parquet;
use crate::plugins::DataSink;
use async_trait::async_trait;
use datafusion::execution::SendableRecordBatchStream;
use object_store::azure::MicrosoftAzureBuilder;
use object_store::path::Path as ObjectPath;
use object_store::ObjectStore;
use serde_derive::Deserialize;
use tracing::info;

#[derive(Debug, Deserialize, Clone)]
pub struct DataSinkAzureBlobPluginConfig {
    pub account_name: String,
    pub account_key: Option<String>,
    pub sas_token: Option<String>,
    pub container: String,
    pub prefix: Option<String>,
    pub format: Option<String>,
}

impl From<DataSinkPluginConfig> for DataSinkAzureBlobPluginConfig {
    fn from(plugin_config: DataSinkPluginConfig) -> Self {
        match plugin_config {
            DataSinkPluginConfig::AzureBlob(config) => config,
            _ => panic!("Invalid plugin type for AzureBlob"),
        }
    }
}

pub struct DataSinkAzureBlobPlugin {
    store: Box<dyn ObjectStore>,
    config: DataSinkAzureBlobPluginConfig,
}

#[async_trait]
impl DataSink for DataSinkAzureBlobPlugin {
    async fn sync(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
    ) -> Result<(), std::io::Error> {
        use crate::metrics::counters;
        counters::inc_uploads_in_flight();

        let namespace = BufferChunker::decode_file_namespace(&filename);
        let prefix = self.config.prefix.as_deref().unwrap_or("").trim_matches('/');

        let mut full_key = if namespace.is_empty() {
            prefix.to_string()
        } else if prefix.is_empty() {
            namespace.clone()
        } else {
            format!("{}/{}", prefix, namespace)
        };

        let partition_path = BufferChunker::decode_file_partition(&filename);
        if !partition_path.is_empty() {
            full_key = format!("{}/{}", full_key, partition_path);
        }

        if let Ok(k) = TimePartitioner::new(&filename).process() {
            full_key = format!("{}/{}", full_key, k);
        }

        let md5_digest = md5::compute(&filename);
        let final_key = format!("{}/{}.parquet", full_key, hex::encode(md5_digest.0));

        let parquet_bytes = serialize_to_parquet(stream)
            .await
            .map_err(|e| { counters::dec_uploads_in_flight(); e })?;

        let path = ObjectPath::from(final_key.clone());
        self.store
            .put(&path, parquet_bytes.bytes.into())
            .await
            .map_err(|e| {
                counters::dec_uploads_in_flight();
                std::io::Error::other(e.to_string())
            })?;

        counters::dec_uploads_in_flight();
        info!("AzureBlob: uploaded {}", final_key);
        Ok(())
    }
}

impl DataSinkAzureBlobPlugin {
    pub async fn new_with_config(
        _buffer_name: String,
        config: Option<DataSinkAzureBlobPluginConfig>,
    ) -> Self {
        let config = config.unwrap_or(DataSinkAzureBlobPluginConfig {
            account_name: Config::getenv("AZURE_STORAGE_ACCOUNT_NAME", ""),
            account_key: Some(Config::getenv("AZURE_STORAGE_ACCOUNT_KEY", "")),
            sas_token: None,
            container: Config::getenv("AZURE_STORAGE_CONTAINER", ""),
            prefix: None,
            format: None,
        });

        let mut builder = MicrosoftAzureBuilder::new()
            .with_account(&config.account_name)
            .with_container_name(&config.container);

        if let Some(ref key) = config.account_key {
            builder = builder.with_access_key(key);
        }
        if let Some(ref sas) = config.sas_token {
            let pairs: Vec<(String, String)> = sas
                .trim_start_matches('?')
                .split('&')
                .filter_map(|pair| {
                    let mut parts = pair.splitn(2, '=');
                    match (parts.next(), parts.next()) {
                        (Some(k), Some(v)) => Some((k.to_string(), v.to_string())),
                        _ => None,
                    }
                })
                .collect();
            builder = builder.with_sas_authorization(pairs);
        }

        let store = builder.build().expect("Failed to build Azure blob store");

        Self {
            store: Box::new(store),
            config,
        }
    }
}
