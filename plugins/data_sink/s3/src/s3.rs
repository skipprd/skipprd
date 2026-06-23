use super::parquet_util::serialize_to_parquet;
use crate::helpers::configuration::DataSinkPluginConfig;
use async_trait::async_trait;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client as S3Client;
use datafusion::execution::SendableRecordBatchStream;
use serde_derive::Deserialize;
use skippr_runtime_sdk::plugins::DataSink;
use skippr_runtime_sdk::sink_compat::partition_time::TimePartitioner;
use skippr_runtime_sdk::sink_compat::BufferChunker;
use std::io;
use tracing::info;

#[derive(Debug, Deserialize, Clone)]
pub struct DataSinkS3PluginConfig {
    pub format: Option<String>,
    pub endpoint_url: Option<String>,
    pub s3_bucket: String,
    pub s3_prefix: String,
}

impl TryFrom<DataSinkPluginConfig> for DataSinkS3PluginConfig {
    type Error = String;

    fn try_from(entry: DataSinkPluginConfig) -> Result<Self, Self::Error> {
        entry.decode_for_plugin("S3")
    }
}

pub struct DataSinkS3Plugin {
    s3_client: S3Client,
    config: DataSinkS3PluginConfig,
}

skippr_runtime_sdk::declare_sink_spec!(
    S3SinkSpec,
    DataSinkS3Plugin,
    skippr_runtime_sdk::plugins::cdc::sink_capabilities::S3,
    skippr_runtime_sdk::plugins::DeterministicObjectOverwrite
);

#[async_trait]
impl DataSink for DataSinkS3Plugin {
    async fn sync(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
        cdc_ctx: Option<&skippr_runtime_sdk::plugins::cdc::SyncContext>,
    ) -> Result<(), std::io::Error> {
        let stream = match cdc_ctx {
            Some(ctx) => super::cdc_encode::augment_stream_with_cdc_columns(stream, &ctx.part_meta),
            None => stream,
        };
        self.inner_sync(stream, filename).await
    }

    async fn sync_with_context(
        &self,
        stream: SendableRecordBatchStream,
        ctx: skippr_runtime_sdk::plugins::SinkWriteContext<'_>,
    ) -> Result<(), std::io::Error> {
        ctx.validate_grouped::<skippr_runtime_sdk::plugins::DeterministicObjectOverwrite>()
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::Unsupported, err))?;
        let stream = match ctx.cdc_ctx {
            Some(cdc) => super::cdc_encode::augment_stream_with_cdc_columns(stream, &cdc.part_meta),
            None => stream,
        };
        let object_stem = if ctx.idempotency_key.is_empty() {
            None
        } else {
            Some(
                skippr_runtime_sdk::sink_idempotency::deterministic_object_name(
                    &ctx.idempotency_key,
                    "",
                )?,
            )
        };
        self.inner_sync_with_object_stem(stream, ctx.filename, object_stem.as_deref())
            .await
    }

    fn capability(&self) -> &'static skippr_runtime_sdk::plugins::cdc::SinkCapability {
        &skippr_runtime_sdk::plugins::cdc::sink_capabilities::S3
    }
}

impl DataSinkS3Plugin {
    pub async fn new_with_config(
        _buffer_name: String,
        config: DataSinkS3PluginConfig,
    ) -> io::Result<DataSinkS3Plugin> {
        let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .load()
            .await;
        let mut s3_client_config = aws_sdk_s3::config::Builder::from(&aws_config);
        if let Some(ref endpoint_url) = config.endpoint_url {
            s3_client_config = s3_client_config
                .endpoint_url(endpoint_url)
                .force_path_style(true);
        }
        let s3_client = S3Client::from_conf(s3_client_config.build());
        Ok(Self { s3_client, config })
    }

    async fn inner_sync(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
    ) -> Result<(), std::io::Error> {
        self.inner_sync_with_object_stem(stream, filename, None)
            .await
    }

    async fn inner_sync_with_object_stem(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
        object_stem: Option<&str>,
    ) -> Result<(), std::io::Error> {
        use skippr_runtime_sdk::metrics::counters;
        counters::inc_uploads_in_flight();

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

        let object_stem = object_stem
            .map(str::to_string)
            .unwrap_or_else(|| hex::encode(md5::compute(&filename).0));
        let final_key = format!("{}/{}.parquet", full_key, object_stem);

        let parquet_bytes = serialize_to_parquet(stream).await.map_err(|e| {
            counters::dec_uploads_in_flight();
            e
        })?;
        let row_count = parquet_bytes.num_rows as u64;
        let byte_count = parquet_bytes.size_bytes;

        self.s3_client
            .put_object()
            .bucket(&self.config.s3_bucket)
            .key(&final_key)
            .body(ByteStream::from(parquet_bytes.bytes))
            .send()
            .await
            .map_err(|e| {
                counters::dec_uploads_in_flight();
                std::io::Error::other(format!("Failed to upload to S3: {e}"))
            })?;

        counters::add_parquet_rows(row_count);
        counters::add_parquet_bytes(byte_count);
        counters::add_upload(1);
        counters::dec_uploads_in_flight();
        info!(
            "Uploaded s3://{}/{} to S3 (rows={}, bytes={}, namespace={}, partition_prefix=s3://{}/{}/)",
            self.config.s3_bucket,
            final_key,
            row_count,
            byte_count,
            namespace,
            self.config.s3_bucket,
            full_key.trim_end_matches('/')
        );
        Ok(())
    }
}
