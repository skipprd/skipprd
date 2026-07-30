use super::parquet_util::serialize_to_parquet;
use crate::helpers::configuration::DataSinkPluginConfig;
use async_trait::async_trait;
use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_s3::operation::get_object::GetObjectError;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client as S3Client;
use datafusion::execution::SendableRecordBatchStream;
use serde_derive::Deserialize;
use skippr_runtime_sdk::plugins::DataSink;
use skippr_runtime_sdk::plugins::{SinkWriteContext, SinkWriteOutcome};
use skippr_runtime_sdk::sink_compat::partition_time::TimePartitioner;
use skippr_runtime_sdk::sink_compat::BufferChunker;
use skippr_runtime_sdk::sink_idempotency::{sidecar_manifest_object_key, ObjectWriteManifest};
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
        ctx: SinkWriteContext<'_>,
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

    async fn sync_with_context_result(
        &self,
        stream: SendableRecordBatchStream,
        ctx: SinkWriteContext<'_>,
    ) -> Result<SinkWriteOutcome, std::io::Error> {
        if !ctx.is_grouped() {
            self.sync_with_context(stream, ctx).await?;
            return Ok(SinkWriteOutcome::Applied);
        }
        ctx.validate_grouped::<skippr_runtime_sdk::plugins::DeterministicObjectOverwrite>()
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::Unsupported, err))?;
        let object_stem = skippr_runtime_sdk::sink_idempotency::deterministic_object_name(
            &ctx.idempotency_key,
            "",
        )?;
        let namespace = BufferChunker::decode_file_namespace(&ctx.filename);
        let manifest_key =
            sidecar_manifest_object_key(&self.config.s3_prefix, &namespace, &object_stem);
        let expected_manifest = ObjectWriteManifest::from_context(
            ctx.compaction_id.clone(),
            ctx.idempotency_key.clone(),
            ctx.schema_fingerprint.clone(),
            &ctx.wal_refs,
        );
        if self
            .manifest_matches(&manifest_key, &expected_manifest)
            .await?
        {
            return Ok(SinkWriteOutcome::AlreadyApplied);
        }
        let ctx_filename = ctx.filename.clone();
        self.inner_sync_with_object_stem(stream, ctx_filename, Some(&object_stem))
            .await?;
        self.write_manifest(&manifest_key, &expected_manifest)
            .await?;
        Ok(SinkWriteOutcome::Applied)
    }

    async fn sync_grouped(
        &self,
        mut reader: skippr_runtime_sdk::plugins::GroupedBatchReader,
        ctx: skippr_runtime_sdk::plugins::GroupedSinkWriteContext<'_>,
    ) -> Result<SinkWriteOutcome, std::io::Error> {
        let schema = reader.schema();
        let mut applied = false;
        while let Some(chunk) = reader.next_chunk().await? {
            let chunk_cdc = ctx.chunk_cdc_context(&chunk)?;
            let chunk_ctx = ctx.chunk_sink_write_context_with_cdc(
                chunk.chunk_index,
                chunk.chunk_index == 0 && chunk.final_chunk,
                chunk_cdc.as_ref(),
            );
            if self
                .sync_with_context_result(chunk.into_stream(schema.clone()), chunk_ctx)
                .await?
                == SinkWriteOutcome::Applied
            {
                applied = true;
            }
        }
        Ok(if applied {
            SinkWriteOutcome::Applied
        } else {
            SinkWriteOutcome::AlreadyApplied
        })
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

        let object_stem = object_stem
            .map(str::to_string)
            .unwrap_or_else(|| hex::encode(md5::compute(&filename).0));
        let final_key = self.object_key_for_filename(&filename, &object_stem);
        let namespace = BufferChunker::decode_file_namespace(&filename);
        let full_key = final_key
            .rsplit_once('/')
            .map(|(prefix, _)| prefix.to_string())
            .unwrap_or_default();

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

    fn object_key_for_filename(&self, filename: &str, object_stem: &str) -> String {
        let namespace = BufferChunker::decode_file_namespace(filename);
        let trimmed_key = self.config.s3_prefix.trim_matches('/').to_string();

        let mut full_key = if namespace.is_empty() {
            trimmed_key
        } else if trimmed_key.is_empty() {
            namespace
        } else {
            format!("{}/{}", trimmed_key, namespace)
        };

        let partition_path = BufferChunker::decode_file_partition(filename);
        if !partition_path.is_empty() {
            full_key = format!("{}/{}", full_key, partition_path);
        }

        let filename_owned = filename.to_string();
        if let Ok(k) = TimePartitioner::new(&filename_owned).process() {
            full_key = format!("{}/{}", full_key, k);
        }

        format!("{}/{}.parquet", full_key, object_stem)
    }

    async fn manifest_matches(
        &self,
        manifest_key: &str,
        expected: &ObjectWriteManifest,
    ) -> io::Result<bool> {
        let response = match self
            .s3_client
            .get_object()
            .bucket(&self.config.s3_bucket)
            .key(manifest_key)
            .send()
            .await
        {
            Ok(response) => response,
            Err(err) => {
                if is_s3_get_object_not_found_error(&err) {
                    return Ok(false);
                }
                return Err(io::Error::other(format!(
                    "Failed to read S3 idempotency manifest {}: {}",
                    manifest_key, err
                )));
            }
        };
        let bytes = response
            .body
            .collect()
            .await
            .map_err(|err| io::Error::other(err.to_string()))?
            .into_bytes();
        let manifest = ObjectWriteManifest::from_json_bytes(&bytes)?;
        Ok(manifest.matches_manifest(expected))
    }

    async fn write_manifest(
        &self,
        manifest_key: &str,
        manifest: &ObjectWriteManifest,
    ) -> io::Result<()> {
        self.s3_client
            .put_object()
            .bucket(&self.config.s3_bucket)
            .key(manifest_key)
            .body(ByteStream::from(manifest.to_json_bytes()?))
            .send()
            .await
            .map_err(|err| {
                io::Error::other(format!(
                    "Failed to write S3 idempotency manifest {}: {}",
                    manifest_key, err
                ))
            })?;
        Ok(())
    }
}

fn is_s3_get_object_not_found_error(err: &SdkError<GetObjectError>) -> bool {
    if err
        .raw_response()
        .is_some_and(|response| response.status().as_u16() == 404)
    {
        return true;
    }
    if let Some(service_error) = err.as_service_error() {
        if is_s3_not_found_code(service_error.code()) {
            return true;
        }
        if service_error
            .message()
            .is_some_and(is_s3_not_found_error_text)
        {
            return true;
        }
    }
    is_s3_not_found_error_text(&err.to_string())
}

fn is_s3_not_found_code(code: Option<&str>) -> bool {
    matches!(
        code,
        Some("NoSuchKey" | "NotFound" | "NotFoundException" | "404")
    )
}

fn is_s3_not_found_error_text(err: &str) -> bool {
    err.contains("NoSuchKey")
        || err.contains("NotFound")
        || err.contains("status code: 404")
        || err.contains("404 Not Found")
}
