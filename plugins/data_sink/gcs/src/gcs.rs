use crate::helpers::configuration::DataSinkPluginConfig;
use async_trait::async_trait;
use datafusion::execution::SendableRecordBatchStream;
use futures::StreamExt;
use object_store::gcp::GoogleCloudStorageBuilder;
use object_store::path::Path as ObjectPath;
use object_store::ObjectStore;
use object_store::ObjectStoreExt;
use serde_derive::Deserialize;
use skippr_object_writer::backends::ObjectStoreBackend;
use skippr_object_writer::{
    ObjectWriteError, ObjectWriteReceipt, ObjectWriteRequest, ObjectWriteSession,
    ObjectWriterConfig,
};
use skippr_runtime_sdk::plugins::DataSink;
use skippr_runtime_sdk::plugins::{SinkWriteContext, SinkWriteOutcome};
use skippr_runtime_sdk::sink_compat::partition_time::TimePartitioner;
use skippr_runtime_sdk::sink_compat::BufferChunker;
use skippr_runtime_sdk::sink_idempotency::{
    legacy_chunk_idempotency_key, manifest_object_name, persisted_object_write_matches,
    GroupedWriteReceipt, ObjectWriteManifest,
};
use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;
use tracing::info;

#[derive(Debug, Deserialize, Clone)]
pub struct DataSinkGcsPluginConfig {
    pub bucket: String,
    pub prefix: Option<String>,
    pub service_account_key_path: Option<String>,
    pub format: Option<String>,
}

impl TryFrom<DataSinkPluginConfig> for DataSinkGcsPluginConfig {
    type Error = String;

    fn try_from(entry: DataSinkPluginConfig) -> Result<Self, Self::Error> {
        entry.decode_for_plugin("Gcs")
    }
}

pub struct DataSinkGcsPlugin {
    store: Arc<dyn ObjectStore>,
    object_backend: Arc<ObjectStoreBackend>,
    config: DataSinkGcsPluginConfig,
}

skippr_runtime_sdk::declare_sink_spec!(
    GcsSinkSpec,
    DataSinkGcsPlugin,
    skippr_runtime_sdk::plugins::cdc::sink_capabilities::GCS,
    skippr_runtime_sdk::plugins::DeterministicObjectOverwrite
);

#[async_trait]
impl DataSink for DataSinkGcsPlugin {
    async fn sync(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
        cdc_ctx: Option<&skippr_runtime_sdk::plugins::cdc::SyncContext>,
    ) -> Result<(), std::io::Error> {
        self.sync_with_context(
            stream,
            skippr_runtime_sdk::plugins::SinkWriteContext {
                filename,
                compaction_id: String::new(),
                idempotency_key: String::new(),
                wal_refs: Vec::new(),
                write_semantics: skippr_runtime_sdk::plugins::SinkWriteSemantics::AtLeastOnce,
                schema_fingerprint: String::new(),
                cdc_ctx,
                source_contract: None,
            },
        )
        .await
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
            hex::encode(md5::compute(&ctx.filename).0)
        } else {
            skippr_runtime_sdk::sink_idempotency::deterministic_object_name(
                &ctx.idempotency_key,
                "",
            )?
        };
        let final_key = self.object_key_for_filename(&ctx.filename, &object_stem);
        self.write_stream(stream, &final_key).await?;
        info!("GCS: uploaded gs://{}/{}", self.config.bucket, final_key);
        Ok(())
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
        let final_key = self.object_key_for_filename(&ctx.filename, &object_stem);
        let manifest_path = ObjectPath::from(manifest_object_name(&final_key));
        let expected_manifest = ObjectWriteManifest::from_context(
            ctx.compaction_id.clone(),
            ctx.idempotency_key.clone(),
            ctx.schema_fingerprint.clone(),
            &ctx.wal_refs,
        );
        if self
            .manifest_matches(&manifest_path, &expected_manifest)
            .await?
        {
            return Ok(SinkWriteOutcome::AlreadyApplied);
        }
        let stream = match ctx.cdc_ctx {
            Some(cdc) => super::cdc_encode::augment_stream_with_cdc_columns(stream, &cdc.part_meta),
            None => stream,
        };
        let receipt = self.write_stream(stream, &final_key).await?;
        self.write_receipt(&manifest_path, &expected_manifest, &receipt)
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
        let final_key = self.object_key_for_filename(&ctx.filename, &object_stem);
        let manifest_path = ObjectPath::from(manifest_object_name(&final_key));
        let expected_manifest = ObjectWriteManifest::from_context(
            ctx.compaction_id.clone(),
            ctx.idempotency_key.clone(),
            ctx.schema_fingerprint.clone(),
            ctx.wal_refs.as_slice(),
        );
        if self
            .manifest_matches(&manifest_path, &expected_manifest)
            .await?
        {
            return Ok(SinkWriteOutcome::AlreadyApplied);
        }
        if self.legacy_chunk_state_exists(&ctx, 0).await? {
            let receipt = self.sync_grouped_legacy_chunks(&mut reader, &ctx).await?;
            self.write_receipt(&manifest_path, &expected_manifest, &receipt)
                .await?;
            return Ok(SinkWriteOutcome::Applied);
        }
        let (stream, _stream_progress) = reader.into_stream();
        let stream = match ctx.cdc_ctx {
            Some(cdc) => super::cdc_encode::augment_stream_with_cdc_columns(stream, &cdc.part_meta),
            None => stream,
        };
        let receipt = self.write_stream(stream, &final_key).await?;
        self.write_receipt(&manifest_path, &expected_manifest, &receipt)
            .await?;
        Ok(SinkWriteOutcome::Applied)
    }

    fn capability(&self) -> &'static skippr_runtime_sdk::plugins::cdc::SinkCapability {
        &skippr_runtime_sdk::plugins::cdc::sink_capabilities::GCS
    }
}

impl DataSinkGcsPlugin {
    pub async fn new_with_config(
        _buffer_name: String,
        config: DataSinkGcsPluginConfig,
    ) -> io::Result<Self> {
        let mut builder = GoogleCloudStorageBuilder::new().with_bucket_name(&config.bucket);

        if let Some(ref key_path) = config.service_account_key_path {
            if !key_path.is_empty() {
                builder = builder.with_service_account_path(key_path);
            }
        }

        let store = builder
            .build()
            .map_err(|err| io::Error::other(format!("Failed to build GCS store: {}", err)))?;
        let store: Arc<dyn ObjectStore> = Arc::new(store);
        let object_backend = Arc::new(ObjectStoreBackend::new(store.clone()));

        Ok(Self {
            store,
            object_backend,
            config,
        })
    }

    fn object_key_for_filename(&self, filename: &str, object_stem: &str) -> String {
        let namespace = BufferChunker::decode_file_namespace(filename);
        let prefix = self
            .config
            .prefix
            .as_deref()
            .unwrap_or("")
            .trim_matches('/');

        let mut full_key = if namespace.is_empty() {
            prefix.to_string()
        } else if prefix.is_empty() {
            namespace
        } else {
            format!("{}/{}", prefix, namespace)
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

    fn legacy_chunk_manifest(
        &self,
        ctx: &skippr_runtime_sdk::plugins::GroupedSinkWriteContext<'_>,
        chunk_index: u64,
    ) -> (String, ObjectPath, ObjectWriteManifest) {
        let object_stem = legacy_chunk_idempotency_key(&ctx.idempotency_key, chunk_index);
        let chunk_filename = ctx.chunk_filename(chunk_index, false);
        let final_key = self.object_key_for_filename(&chunk_filename, &object_stem);
        let manifest_path = ObjectPath::from(manifest_object_name(&final_key));
        let manifest = ObjectWriteManifest::from_context(
            legacy_chunk_idempotency_key(&ctx.compaction_id, chunk_index),
            object_stem,
            ctx.schema_fingerprint.clone(),
            ctx.wal_refs.as_slice(),
        );
        (final_key, manifest_path, manifest)
    }

    async fn legacy_chunk_state_exists(
        &self,
        ctx: &skippr_runtime_sdk::plugins::GroupedSinkWriteContext<'_>,
        chunk_index: u64,
    ) -> io::Result<bool> {
        let (final_key, path, expected) = self.legacy_chunk_manifest(ctx, chunk_index);
        if self.manifest_matches(&path, &expected).await? {
            return Ok(true);
        }
        match self.store.head(&ObjectPath::from(final_key)).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
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

        while let Some(chunk) = reader.next_chunk().await? {
            let (final_key, manifest_path, expected) =
                self.legacy_chunk_manifest(ctx, chunk.chunk_index);
            let chunk_rows = chunk.rows;
            let (chunk_bytes, chunk_transport_count, chunk_etag) =
                if self.manifest_matches(&manifest_path, &expected).await? {
                    let head = self
                        .store
                        .head(&ObjectPath::from(final_key.clone()))
                        .await
                        .map_err(|error| io::Error::other(error.to_string()))?;
                    (head.size, 1, head.e_tag)
                } else {
                    let chunk_cdc = ctx.chunk_cdc_context(&chunk)?;
                    let stream = chunk.into_stream(schema.clone());
                    let stream = match chunk_cdc.as_ref() {
                        Some(cdc) => super::cdc_encode::augment_stream_with_cdc_columns(
                            stream,
                            &cdc.part_meta,
                        ),
                        None => stream,
                    };
                    let receipt = self.write_stream(stream, &final_key).await?;
                    (receipt.bytes, receipt.transport_chunk_count, receipt.etag)
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
            checksum: None,
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
            skippr_runtime_sdk::converters::parquet_ordering::resolve_effective_order(&schema);
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
        manifest_path: &ObjectPath,
        expected: &ObjectWriteManifest,
    ) -> io::Result<bool> {
        let result = match self.store.get(manifest_path).await {
            Ok(result) => result,
            Err(object_store::Error::NotFound { .. }) => return Ok(false),
            Err(err) => return Err(io::Error::other(err.to_string())),
        };
        let bytes = result
            .bytes()
            .await
            .map_err(|err| io::Error::other(err.to_string()))?;
        persisted_object_write_matches(&bytes, expected)
    }

    async fn write_receipt(
        &self,
        manifest_path: &ObjectPath,
        manifest: &ObjectWriteManifest,
        object_receipt: &ObjectWriteReceipt,
    ) -> io::Result<()> {
        let receipt = GroupedWriteReceipt::from_manifest_and_upload(
            manifest,
            format!("gs://{}/{}", self.config.bucket, object_receipt.object_key),
            object_receipt.etag.clone().unwrap_or_default(),
            object_receipt.checksum.clone(),
            object_receipt.rows,
            object_receipt.bytes,
            object_receipt.transport_chunk_count,
        );
        self.store
            .put(manifest_path, receipt.to_json_bytes()?.into())
            .await
            .map_err(|error| io::Error::other(error.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use arrow::array::{Int64Array, RecordBatch};
    use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
    use datafusion::error::DataFusionError;
    use datafusion::physical_plan::RecordBatchStream;
    use futures::{Stream, TryStreamExt};
    use object_store::memory::InMemory;
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    use skippr_runtime_sdk::plugins::cdc::{MutationKind, SyncContext, WalPartMeta, WalRowMeta};
    use skippr_runtime_sdk::plugins::{
        GroupedBatchReader, GroupedBatchReaderConfig, GroupedSinkWriteContext, SinkWriteSemantics,
    };

    use super::*;

    struct VecBatchStream {
        schema: SchemaRef,
        batches: std::vec::IntoIter<RecordBatch>,
    }

    impl Stream for VecBatchStream {
        type Item = Result<RecordBatch, DataFusionError>;

        fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Ready(self.batches.next().map(Ok))
        }
    }

    impl RecordBatchStream for VecBatchStream {
        fn schema(&self) -> SchemaRef {
            self.schema.clone()
        }
    }

    fn test_plugin(store: Arc<dyn ObjectStore>) -> DataSinkGcsPlugin {
        DataSinkGcsPlugin {
            store: store.clone(),
            object_backend: Arc::new(ObjectStoreBackend::new(store)),
            config: DataSinkGcsPluginConfig {
                bucket: "bucket".to_string(),
                prefix: Some("/root/".to_string()),
                service_account_key_path: None,
                format: None,
            },
        }
    }

    #[test]
    fn deterministic_object_path_is_unchanged() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let plugin = test_plugin(store);

        assert_eq!(
            plugin.object_key_for_filename("namespace=events", "apply-0001"),
            "root/events/apply-0001.parquet"
        );
    }

    #[tokio::test]
    async fn grouped_chunks_write_one_object_with_cdc_and_replay_receipt() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let plugin = test_plugin(store.clone());
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        let make_reader = || {
            let batches = vec![
                RecordBatch::try_new(schema.clone(), vec![Arc::new(Int64Array::from(vec![1]))])
                    .unwrap(),
                RecordBatch::try_new(schema.clone(), vec![Arc::new(Int64Array::from(vec![2]))])
                    .unwrap(),
            ];
            let stream: SendableRecordBatchStream = Box::pin(VecBatchStream {
                schema: schema.clone(),
                batches: batches.into_iter(),
            });
            let refs = skippr_runtime_sdk::plugins::GroupedWalRefs::new(vec![
                skippr_runtime_sdk::protocol::RuntimeWalPartRef {
                    segment_id: "segment".to_string(),
                    source: "local".to_string(),
                    start: 0,
                    len: 2,
                    sink_ref: "sink".to_string(),
                    namespace: "events".to_string(),
                    partition: String::new(),
                    time: None,
                    schema_fingerprint: "schema".to_string(),
                    cdc_meta_hash: Some([7; 32]),
                },
            ])
            .unwrap();
            let key = skippr_runtime_sdk::plugins::GroupedWalPartitionKey::from_refs(
                &refs, "schema", None,
            );
            GroupedBatchReader::new(
                stream,
                key,
                GroupedBatchReaderConfig {
                    max_rows: 1,
                    max_bytes: usize::MAX,
                },
            )
        };
        let cdc = SyncContext {
            part_meta: WalPartMeta::cdc(
                vec![
                    WalRowMeta {
                        mutation: MutationKind::Insert,
                        event_id: b"one".to_vec(),
                        order_token: vec![1],
                    },
                    WalRowMeta {
                        mutation: MutationKind::Delete,
                        event_id: b"two".to_vec(),
                        order_token: vec![2],
                    },
                ],
                2,
            )
            .unwrap(),
            contract: None,
        };
        let make_context = || {
            GroupedSinkWriteContext::try_from(SinkWriteContext {
                filename: "namespace=events".to_string(),
                compaction_id: "apply-0001".to_string(),
                idempotency_key: "apply-0001".to_string(),
                wal_refs: vec![skippr_runtime_sdk::protocol::RuntimeWalPartRef {
                    segment_id: "segment".to_string(),
                    source: "local".to_string(),
                    start: 0,
                    len: 2,
                    sink_ref: "sink".to_string(),
                    namespace: "events".to_string(),
                    partition: String::new(),
                    time: None,
                    schema_fingerprint: "schema".to_string(),
                    cdc_meta_hash: Some([7; 32]),
                }],
                write_semantics: SinkWriteSemantics::IdempotentAtLeastOnce,
                schema_fingerprint: "schema".to_string(),
                cdc_ctx: Some(&cdc),
                source_contract: None,
            })
            .unwrap()
        };

        assert_eq!(
            plugin
                .sync_grouped(make_reader(), make_context())
                .await
                .unwrap(),
            SinkWriteOutcome::Applied
        );
        let objects = store.list(None).try_collect::<Vec<_>>().await.unwrap();
        let parquet_objects = objects
            .iter()
            .filter(|object| object.location.as_ref().ends_with(".parquet"))
            .collect::<Vec<_>>();
        assert_eq!(parquet_objects.len(), 1);
        assert_eq!(
            parquet_objects[0].location.as_ref(),
            "root/events/apply-0001.parquet"
        );
        assert!(!objects
            .iter()
            .any(|object| object.location.as_ref().contains("-chunk-")));

        let parquet_bytes = store
            .get(&parquet_objects[0].location)
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        let batches = ParquetRecordBatchReaderBuilder::try_new(parquet_bytes)
            .unwrap()
            .build()
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(batches.iter().map(RecordBatch::num_rows).sum::<usize>(), 2);
        assert_eq!(batches[0].schema().field(1).name(), "_skippr_mutation");

        assert_eq!(
            plugin
                .sync_grouped(make_reader(), make_context())
                .await
                .unwrap(),
            SinkWriteOutcome::AlreadyApplied
        );
        assert_eq!(
            store
                .list(None)
                .try_collect::<Vec<_>>()
                .await
                .unwrap()
                .len(),
            2
        );
    }
}
