use crate::helpers::configuration::DataSinkPluginConfig;
use async_trait::async_trait;
use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_s3::operation::get_object::GetObjectError;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use aws_sdk_s3::Client as S3Client;
use datafusion::execution::SendableRecordBatchStream;
use futures::StreamExt;
use serde_derive::Deserialize;
use skippr_object_writer::{
    CompletionMetadata, MultipartUpload, ObjectPartReceipt, ObjectWriteBackend, ObjectWriteError,
    ObjectWriteReceipt, ObjectWriteRequest, ObjectWriteSession, ObjectWriterConfig, PartMetadata,
};
use skippr_runtime_sdk::plugins::DataSink;
use skippr_runtime_sdk::plugins::{SinkPreflightOutcome, SinkWriteContext, SinkWriteOutcome};
use skippr_runtime_sdk::sink_compat::partition_time::TimePartitioner;
use skippr_runtime_sdk::sink_compat::BufferChunker;
use skippr_runtime_sdk::sink_idempotency::{
    legacy_chunk_idempotency_key, persisted_object_write_matches, sidecar_manifest_object_key,
    GroupedWriteReceipt, ObjectWriteManifest,
};
use skippr_runtime_sdk::SkipprConfig;
use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;
use tracing::info;

#[derive(Debug, Deserialize, SkipprConfig, Clone)]
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
    object_backend: Arc<S3ObjectBackend>,
    config: DataSinkS3PluginConfig,
}

struct S3ObjectBackend {
    client: S3Client,
    bucket: String,
}

#[async_trait]
impl ObjectWriteBackend for S3ObjectBackend {
    type Error = io::Error;

    async fn begin(&self, request: &ObjectWriteRequest) -> Result<MultipartUpload, Self::Error> {
        let response = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(&request.object_key)
            .content_type(&request.content_type)
            .set_metadata((!request.metadata.is_empty()).then(|| {
                request
                    .metadata
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect()
            }))
            .send()
            .await
            .map_err(|error| io::Error::other(format!("Failed to begin S3 upload: {error}")))?;
        let upload_id = response
            .upload_id()
            .ok_or_else(|| io::Error::other("S3 multipart upload returned no upload id"))?
            .to_string();
        Ok(MultipartUpload {
            upload_id,
            metadata: BTreeMap::from([
                ("bucket".to_string(), self.bucket.clone()),
                ("object_key".to_string(), request.object_key.clone()),
            ]),
        })
    }

    async fn upload_part(
        &self,
        upload: &MultipartUpload,
        part_number: u32,
        bytes: bytes::Bytes,
    ) -> Result<PartMetadata, Self::Error> {
        let part_number = i32::try_from(part_number)
            .map_err(|_| io::Error::other("S3 part number exceeds i32::MAX"))?;
        let response = self
            .client
            .upload_part()
            .bucket(&self.bucket)
            .key(
                upload
                    .metadata
                    .get("object_key")
                    .ok_or_else(|| io::Error::other("S3 upload is missing object key"))?,
            )
            .upload_id(&upload.upload_id)
            .part_number(part_number)
            .body(ByteStream::from(bytes))
            .send()
            .await
            .map_err(|error| io::Error::other(format!("Failed to upload S3 part: {error}")))?;
        Ok(PartMetadata {
            etag: response.e_tag().map(str::to_string),
            checksum: response.checksum_sha256().map(str::to_string),
            metadata: BTreeMap::new(),
        })
    }

    async fn complete(
        &self,
        upload: &MultipartUpload,
        parts: &[ObjectPartReceipt],
    ) -> Result<CompletionMetadata, Self::Error> {
        let object_key = upload
            .metadata
            .get("object_key")
            .ok_or_else(|| io::Error::other("S3 upload is missing object key"))?;
        let completed_parts = parts
            .iter()
            .map(|part| {
                let part_number = i32::try_from(part.part_number)
                    .map_err(|_| io::Error::other("S3 part number exceeds i32::MAX"))?;
                Ok(CompletedPart::builder()
                    .set_e_tag(part.etag.clone())
                    .part_number(part_number)
                    .build())
            })
            .collect::<io::Result<Vec<_>>>()?;
        let response = self
            .client
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(object_key)
            .upload_id(&upload.upload_id)
            .multipart_upload(
                CompletedMultipartUpload::builder()
                    .set_parts(Some(completed_parts))
                    .build(),
            )
            .send()
            .await
            .map_err(|error| io::Error::other(format!("Failed to complete S3 upload: {error}")))?;
        Ok(CompletionMetadata {
            etag: response.e_tag().map(str::to_string),
            checksum: response.checksum_sha256().map(str::to_string),
            version_id: response.version_id().map(str::to_string),
            metadata: BTreeMap::new(),
        })
    }

    async fn abort(&self, upload: &MultipartUpload) -> Result<(), Self::Error> {
        let Some(object_key) = upload.metadata.get("object_key") else {
            return Ok(());
        };
        self.client
            .abort_multipart_upload()
            .bucket(&self.bucket)
            .key(object_key)
            .upload_id(&upload.upload_id)
            .send()
            .await
            .map_err(|error| io::Error::other(format!("Failed to abort S3 upload: {error}")))?;
        Ok(())
    }
}

skippr_runtime_sdk::declare_sink_spec!(
    S3SinkSpec,
    DataSinkS3Plugin,
    skippr_runtime_sdk::plugins::cdc::sink_capabilities::S3,
    skippr_runtime_sdk::plugins::DeterministicObjectOverwrite
);

#[async_trait]
impl DataSink for DataSinkS3Plugin {
    async fn preflight(
        &self,
        ctx: SinkWriteContext<'_>,
    ) -> Result<SinkPreflightOutcome, std::io::Error> {
        if !ctx.is_grouped() {
            return Ok(SinkPreflightOutcome::Ready);
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
            ctx.compaction_id,
            ctx.idempotency_key,
            ctx.schema_fingerprint,
            &ctx.wal_refs,
        );
        if self
            .manifest_matches(&manifest_key, &expected_manifest)
            .await?
        {
            return Ok(SinkPreflightOutcome::AlreadyApplied {
                authority: format!("s3://{}/{}", self.config.s3_bucket, manifest_key),
            });
        }
        Ok(SinkPreflightOutcome::Ready)
    }

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
        let final_key = self.object_key_for_filename(&ctx.filename, &object_stem);
        let stream = match ctx.cdc_ctx {
            Some(cdc) => super::cdc_encode::augment_stream_with_cdc_columns(stream, &cdc.part_meta),
            None => stream,
        };
        let receipt = self.write_stream(stream, &final_key).await?;
        self.write_receipt(&manifest_key, &expected_manifest, &receipt)
            .await?;
        Ok(SinkWriteOutcome::Applied)
    }

    async fn sync_grouped(
        &self,
        mut reader: skippr_runtime_sdk::plugins::GroupedBatchReader,
        ctx: skippr_runtime_sdk::plugins::GroupedSinkWriteContext<'_>,
    ) -> Result<SinkWriteOutcome, std::io::Error> {
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
            ctx.wal_refs.as_slice(),
        );
        if self
            .manifest_matches(&manifest_key, &expected_manifest)
            .await?
        {
            return Ok(SinkWriteOutcome::AlreadyApplied);
        }
        if self.legacy_chunk_state_exists(&ctx, 0).await? {
            let receipt = self.sync_grouped_legacy_chunks(&mut reader, &ctx).await?;
            self.write_receipt(&manifest_key, &expected_manifest, &receipt)
                .await?;
            return Ok(SinkWriteOutcome::Applied);
        }
        let final_key = self.object_key_for_filename(&ctx.filename, &object_stem);
        let (stream, _stream_progress) = reader.into_stream();
        let stream = match ctx.cdc_ctx {
            Some(cdc) => super::cdc_encode::augment_stream_with_cdc_columns(stream, &cdc.part_meta),
            None => stream,
        };
        let receipt = self.write_stream(stream, &final_key).await?;
        self.write_receipt(&manifest_key, &expected_manifest, &receipt)
            .await?;
        Ok(SinkWriteOutcome::Applied)
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
        let object_backend = Arc::new(S3ObjectBackend {
            client: s3_client.clone(),
            bucket: config.s3_bucket.clone(),
        });
        Ok(Self {
            s3_client,
            object_backend,
            config,
        })
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
        let object_stem = object_stem
            .map(str::to_string)
            .unwrap_or_else(|| hex::encode(md5::compute(&filename).0));
        let final_key = self.object_key_for_filename(&filename, &object_stem);
        let namespace = BufferChunker::decode_file_namespace(&filename);
        let full_key = final_key
            .rsplit_once('/')
            .map(|(prefix, _)| prefix.to_string())
            .unwrap_or_default();

        let receipt = self.write_stream(stream, &final_key).await?;
        info!(
            "Uploaded s3://{}/{} to S3 (rows={}, bytes={}, namespace={}, partition_prefix=s3://{}/{}/)",
            self.config.s3_bucket,
            final_key,
            receipt.rows,
            receipt.bytes,
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
        if let Ok(k) = TimePartitioner::new(&filename_owned)
            .process(&skippr_runtime_sdk::helpers::configuration::Config::new())
        {
            full_key = format!("{}/{}", full_key, k);
        }

        format!("{}/{}.parquet", full_key, object_stem)
    }

    fn legacy_chunk_manifest(
        &self,
        ctx: &skippr_runtime_sdk::plugins::GroupedSinkWriteContext<'_>,
        chunk_index: u64,
    ) -> (String, String, ObjectWriteManifest) {
        let object_stem = legacy_chunk_idempotency_key(&ctx.idempotency_key, chunk_index);
        let namespace = BufferChunker::decode_file_namespace(&ctx.filename);
        let manifest_key =
            sidecar_manifest_object_key(&self.config.s3_prefix, &namespace, &object_stem);
        let manifest = ObjectWriteManifest::from_context(
            legacy_chunk_idempotency_key(&ctx.compaction_id, chunk_index),
            object_stem.clone(),
            ctx.schema_fingerprint.clone(),
            ctx.wal_refs.as_slice(),
        );
        (object_stem, manifest_key, manifest)
    }

    async fn legacy_chunk_state_exists(
        &self,
        ctx: &skippr_runtime_sdk::plugins::GroupedSinkWriteContext<'_>,
        chunk_index: u64,
    ) -> io::Result<bool> {
        let (object_stem, key, expected) = self.legacy_chunk_manifest(ctx, chunk_index);
        if self.manifest_matches(&key, &expected).await? {
            return Ok(true);
        }
        let chunk_filename = ctx.chunk_filename(chunk_index, false);
        let final_key = self.object_key_for_filename(&chunk_filename, &object_stem);
        match self
            .s3_client
            .head_object()
            .bucket(&self.config.s3_bucket)
            .key(final_key)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(error) if is_s3_not_found_error_text(&error.to_string()) => Ok(false),
            Err(error) => Err(io::Error::other(error.to_string())),
        }
    }

    async fn sync_grouped_legacy_chunks(
        &self,
        reader: &mut skippr_runtime_sdk::plugins::GroupedBatchReader,
        ctx: &skippr_runtime_sdk::plugins::GroupedSinkWriteContext<'_>,
    ) -> io::Result<ObjectWriteReceipt> {
        let schema = reader.schema();
        let mut object_key = None;
        let mut rows = 0_u64;
        let mut bytes = 0_u64;
        let mut transport_chunk_count = 0_u32;
        let mut etag = None;
        let mut checksum = None;

        while let Some(chunk) = reader.next_chunk().await? {
            let (object_stem, manifest_key, expected) =
                self.legacy_chunk_manifest(ctx, chunk.chunk_index);
            let chunk_filename = ctx.chunk_filename(chunk.chunk_index, false);
            let final_key = self.object_key_for_filename(&chunk_filename, &object_stem);
            let chunk_rows = chunk.rows;
            let (chunk_bytes, chunk_transport_count, chunk_etag, chunk_checksum) = if self
                .manifest_matches(&manifest_key, &expected)
                .await?
            {
                let head = self
                    .s3_client
                    .head_object()
                    .bucket(&self.config.s3_bucket)
                    .key(&final_key)
                    .send()
                    .await
                    .map_err(|error| io::Error::other(error.to_string()))?;
                (
                    u64::try_from(head.content_length().unwrap_or_default()).unwrap_or_default(),
                    1,
                    head.e_tag().map(str::to_string),
                    head.checksum_sha256().map(str::to_string),
                )
            } else {
                let chunk_cdc = ctx.chunk_cdc_context(&chunk)?;
                let stream = chunk.into_stream(schema.clone());
                let stream = match chunk_cdc.as_ref() {
                    Some(cdc) => {
                        super::cdc_encode::augment_stream_with_cdc_columns(stream, &cdc.part_meta)
                    }
                    None => stream,
                };
                let receipt = self.write_stream(stream, &final_key).await?;
                (
                    receipt.bytes,
                    receipt.transport_chunk_count,
                    receipt.etag,
                    receipt.checksum,
                )
            };
            object_key.get_or_insert(final_key);
            rows = rows
                .checked_add(chunk_rows)
                .ok_or_else(|| io::Error::other("legacy grouped row count overflow"))?;
            bytes = bytes
                .checked_add(chunk_bytes)
                .ok_or_else(|| io::Error::other("legacy grouped byte count overflow"))?;
            transport_chunk_count = transport_chunk_count
                .checked_add(chunk_transport_count)
                .ok_or_else(|| io::Error::other("legacy grouped chunk count overflow"))?;
            etag = chunk_etag;
            checksum = chunk_checksum;
        }

        Ok(ObjectWriteReceipt {
            version: 1,
            object_key: object_key
                .ok_or_else(|| io::Error::other("No rows to write to parquet"))?,
            upload_id: "legacy-chunk-replay".to_string(),
            rows,
            bytes,
            transport_chunk_count,
            parts: Vec::new(),
            etag,
            checksum,
            version_id: None,
            backend_metadata: BTreeMap::from([(
                "compatibility".to_string(),
                "legacy-read-only-chunks".to_string(),
            )]),
        })
    }

    async fn write_stream(
        &self,
        stream: SendableRecordBatchStream,
        final_key: &str,
    ) -> io::Result<ObjectWriteReceipt> {
        use skippr_runtime_sdk::metrics::counters;

        let schema = stream.schema();
        let order_fields =
            skippr_runtime_sdk::converters::parquet_ordering::resolve_effective_order(
                &skippr_runtime_sdk::helpers::configuration::Config::new(),
                &schema,
            );
        let writer_properties =
            skippr_runtime_sdk::converters::parquet_ordering::build_writer_properties(
                &schema,
                &order_fields,
                skippr_runtime_sdk::converters::parquet_ordering::default_streaming_row_group_size(
                ),
            );
        let batches = stream.map(move |batch| {
            let batch = batch.map_err(ObjectWriteError::input)?;
            skippr_runtime_sdk::converters::parquet_ordering::sort_batch(&batch, &order_fields)
                .map_err(ObjectWriteError::input)
        });
        let session = ObjectWriteSession::new(
            self.object_backend.clone(),
            ObjectWriteRequest::new(final_key),
            ObjectWriterConfig::default(),
        )
        .map_err(|error| io::Error::other(error.to_string()))?;

        counters::inc_uploads_in_flight();
        let result = session
            .write_parquet(schema, writer_properties, batches)
            .await;
        counters::dec_uploads_in_flight();
        let receipt = result.map_err(|error| io::Error::other(error.to_string()))?;
        counters::add_parquet_rows(receipt.rows);
        counters::add_parquet_bytes(receipt.bytes);
        counters::add_upload(1);
        Ok(receipt)
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
        persisted_object_write_matches(&bytes, expected)
    }

    async fn write_receipt(
        &self,
        manifest_key: &str,
        manifest: &ObjectWriteManifest,
        object_receipt: &ObjectWriteReceipt,
    ) -> io::Result<()> {
        let receipt = GroupedWriteReceipt::from_manifest_and_upload(
            manifest,
            format!(
                "s3://{}/{}",
                self.config.s3_bucket, object_receipt.object_key
            ),
            object_receipt.etag.clone().unwrap_or_default(),
            object_receipt.checksum.clone(),
            object_receipt.rows,
            object_receipt.bytes,
            object_receipt.transport_chunk_count,
        );
        self.s3_client
            .put_object()
            .bucket(&self.config.s3_bucket)
            .key(manifest_key)
            .body(ByteStream::from(receipt.to_json_bytes()?))
            .send()
            .await
            .map_err(|err| {
                io::Error::other(format!(
                    "Failed to write S3 grouped receipt {}: {}",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_object_path_is_unchanged() {
        let client = S3Client::from_conf(
            aws_sdk_s3::config::Builder::new()
                .behavior_version(aws_config::BehaviorVersion::latest())
                .build(),
        );
        let config = DataSinkS3PluginConfig {
            format: None,
            endpoint_url: None,
            s3_bucket: "bucket".to_string(),
            s3_prefix: "/root/".to_string(),
        };
        let plugin = DataSinkS3Plugin {
            s3_client: client.clone(),
            object_backend: Arc::new(S3ObjectBackend {
                client,
                bucket: config.s3_bucket.clone(),
            }),
            config,
        };

        assert_eq!(
            plugin.object_key_for_filename("namespace=events", "apply-0001"),
            "root/events/apply-0001.parquet"
        );
    }
}
