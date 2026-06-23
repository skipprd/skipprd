use super::parquet_util::serialize_to_parquet;
use crate::helpers::configuration::DataSinkPluginConfig;
use async_trait::async_trait;
use datafusion::execution::SendableRecordBatchStream;
use object_store::gcp::GoogleCloudStorageBuilder;
use object_store::path::Path as ObjectPath;
use object_store::ObjectStore;
use object_store::ObjectStoreExt;
use serde_derive::Deserialize;
use skippr_runtime_sdk::plugins::DataSink;
use skippr_runtime_sdk::sink_compat::partition_time::TimePartitioner;
use skippr_runtime_sdk::sink_compat::BufferChunker;
use std::io;
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
    store: Box<dyn ObjectStore>,
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
        ctx: skippr_runtime_sdk::plugins::SinkWriteContext<'_>,
    ) -> Result<(), std::io::Error> {
        ctx.validate_grouped::<skippr_runtime_sdk::plugins::DeterministicObjectOverwrite>()
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::Unsupported, err))?;
        let stream = match ctx.cdc_ctx {
            Some(cdc) => super::cdc_encode::augment_stream_with_cdc_columns(stream, &cdc.part_meta),
            None => stream,
        };
        use skippr_runtime_sdk::metrics::counters;
        counters::inc_uploads_in_flight();

        let namespace = BufferChunker::decode_file_namespace(&ctx.filename);
        let prefix = self
            .config
            .prefix
            .as_deref()
            .unwrap_or("")
            .trim_matches('/');

        let mut full_key = if namespace.is_empty() {
            prefix.to_string()
        } else if prefix.is_empty() {
            namespace.clone()
        } else {
            format!("{}/{}", prefix, namespace)
        };

        let partition_path = BufferChunker::decode_file_partition(&ctx.filename);
        if !partition_path.is_empty() {
            full_key = format!("{}/{}", full_key, partition_path);
        }

        if let Ok(k) = TimePartitioner::new(&ctx.filename).process() {
            full_key = format!("{}/{}", full_key, k);
        }

        let object_stem = if ctx.idempotency_key.is_empty() {
            hex::encode(md5::compute(&ctx.filename).0)
        } else {
            skippr_runtime_sdk::sink_idempotency::deterministic_object_name(
                &ctx.idempotency_key,
                "",
            )?
        };
        let final_key = format!("{}/{}.parquet", full_key, object_stem);

        let parquet_bytes = serialize_to_parquet(stream).await.map_err(|e| {
            counters::dec_uploads_in_flight();
            e
        })?;

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

        Ok(Self {
            store: Box::new(store),
            config,
        })
    }
}
