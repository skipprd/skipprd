use crate::buffer::BufferChunker;
use crate::helpers::configuration::{Config, OutputPluginConfig};
use crate::ingest::partition_time::TimePartitioner;
use crate::plugins::athena::DataOutputAwsAthenaPlugin;
use crate::plugins::DataOutputPlugin;
use async_trait::async_trait;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client as S3Client;
use datafusion::execution::SendableRecordBatchStream;
use serde_derive::Deserialize;
use tracing::info;

#[derive(Debug, Deserialize, Clone)]
pub struct DataOutputS3PluginConfig {
    pub format: Option<String>,
    pub s3_bucket: String,
    pub s3_prefix: String,
}

impl From<OutputPluginConfig> for DataOutputS3PluginConfig {
    fn from(plugin_config: OutputPluginConfig) -> Self {
        match plugin_config {
            OutputPluginConfig::S3(s3_config) => s3_config,
            _ => panic!("Invalid plugin type"),
        }
    }
}

pub struct DataOutputS3Plugin {
    s3_client: S3Client,
    config: DataOutputS3PluginConfig,
}

#[async_trait]
impl DataOutputPlugin for DataOutputS3Plugin {
    async fn sync(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
    ) -> Result<(), std::io::Error> {
        self.inner_sync(stream, filename).await
    }
}

impl DataOutputS3Plugin {
    pub async fn new(buffer_name: String) -> DataOutputS3Plugin {
        let output_config = Config::get_pipeline_output_plugin_config()
            .ok()
            .and_then(|config| match config {
                OutputPluginConfig::S3(s3_config) => Some(s3_config),
                _ => None,
            });
        Self::new_with_config(buffer_name, output_config).await
    }

    pub async fn new_with_config(
        _buffer_name: String,
        output_config: Option<DataOutputS3PluginConfig>,
    ) -> DataOutputS3Plugin {
        let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .load()
            .await;
        let s3_client = S3Client::new(&aws_config);
        let config = output_config.unwrap_or(DataOutputS3PluginConfig {
            format: None,
            s3_bucket: Config::getenv("DATA_OUTPUT_S3_BUCKET", ""),
            s3_prefix: Config::getenv("DATA_OUTPUT_S3_PREFIX", ""),
        });
        Self { s3_client, config }
    }

    async fn inner_sync(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
    ) -> Result<(), std::io::Error> {
        let namespace = BufferChunker::decode_file_namespace(&filename);
        let trimmed_key = self.config.s3_prefix.trim_matches('/').to_string();

        let mut full_key = if namespace.is_empty() {
            trimmed_key.clone()
        } else if trimmed_key.is_empty() {
            namespace.clone()
        } else {
            format!("{}/{}", trimmed_key, namespace)
        };

        let partition_path = BufferChunker::decode_file_partition(&filename);
        if !partition_path.is_empty() {
            full_key = format!("{}/{}", full_key, partition_path);
        }

        let _key = match TimePartitioner::new(&filename).process() {
            Ok(k) => {
                full_key = format!("{}/{}", full_key, k);
            }
            Err(_e) => {}
        };

        let md5_digest = md5::compute(&filename);
        let final_key = format!("{}/{}.parquet", full_key, hex::encode(&md5_digest.0));

        let parquet_bytes = DataOutputAwsAthenaPlugin::serialize_to_parquet(stream).await?;
        self.s3_client
            .put_object()
            .bucket(&self.config.s3_bucket)
            .key(&final_key)
            .body(ByteStream::from(parquet_bytes.bytes))
            .send()
            .await
            .map_err(|e| std::io::Error::other(format!("Failed to upload to S3: {e}")))?;

        info!("Uploaded to S3: {}", final_key);
        Ok(())
    }
}
