use crate::buffer::BufferChunker;
use crate::helpers::configuration::{Config, DataSinkPluginConfig};
use crate::ingest::partition_time::TimePartitioner;
use crate::plugins::parquet_util::serialize_to_parquet;
use crate::plugins::DataSink;
use async_trait::async_trait;
use datafusion::execution::SendableRecordBatchStream;
use object_store::gcp::GoogleCloudStorageBuilder;
use object_store::path::Path as ObjectPath;
use object_store::ObjectStore;
use serde_derive::Deserialize;
use tracing::info;

#[derive(Debug, Deserialize, Clone)]
pub struct DataSinkGcsPluginConfig {
    pub bucket: String,
    pub prefix: Option<String>,
    pub service_account_key_path: Option<String>,
    pub format: Option<String>,
}

impl From<DataSinkPluginConfig> for DataSinkGcsPluginConfig {
    fn from(plugin_config: DataSinkPluginConfig) -> Self {
        match plugin_config {
            DataSinkPluginConfig::Gcs(config) => config,
            _ => panic!("Invalid plugin type for GCS"),
        }
    }
}

pub struct DataSinkGcsPlugin {
    store: Box<dyn ObjectStore>,
    config: DataSinkGcsPluginConfig,
}

#[async_trait]
impl DataSink for DataSinkGcsPlugin {
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
        info!("GCS: uploaded gs://{}/{}", self.config.bucket, final_key);
        Ok(())
    }
}

impl DataSinkGcsPlugin {
    pub async fn new_with_config(
        _buffer_name: String,
        config: Option<DataSinkGcsPluginConfig>,
    ) -> Self {
        let config = config.unwrap_or(DataSinkGcsPluginConfig {
            bucket: Config::getenv("GCS_BUCKET", ""),
            prefix: None,
            service_account_key_path: None,
            format: None,
        });

        let mut builder = GoogleCloudStorageBuilder::new().with_bucket_name(&config.bucket);

        if let Some(ref key_path) = config.service_account_key_path {
            if !key_path.is_empty() {
                builder = builder.with_service_account_path(key_path);
            }
        }

        let store = builder.build().expect("Failed to build GCS store");

        Self {
            store: Box::new(store),
            config,
        }
    }
}
