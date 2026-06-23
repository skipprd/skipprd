use super::parquet_util::serialize_to_parquet;
use crate::helpers::configuration::DataSinkPluginConfig;
use async_trait::async_trait;
use datafusion::execution::SendableRecordBatchStream;
use object_store::azure::MicrosoftAzureBuilder;
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
pub struct DataSinkAzureBlobPluginConfig {
    pub account_name: String,
    pub account_key: Option<String>,
    pub sas_token: Option<String>,
    pub container: String,
    pub prefix: Option<String>,
    pub format: Option<String>,
}

impl TryFrom<DataSinkPluginConfig> for DataSinkAzureBlobPluginConfig {
    type Error = String;

    fn try_from(entry: DataSinkPluginConfig) -> Result<Self, Self::Error> {
        entry.decode_for_plugin("AzureBlob")
    }
}

pub struct DataSinkAzureBlobPlugin {
    store: Box<dyn ObjectStore>,
    config: DataSinkAzureBlobPluginConfig,
}

skippr_runtime_sdk::declare_sink_spec!(
    AzureBlobSinkSpec,
    DataSinkAzureBlobPlugin,
    skippr_runtime_sdk::plugins::cdc::sink_capabilities::AZURE_BLOB,
    skippr_runtime_sdk::plugins::DeterministicObjectOverwrite
);

#[async_trait]
impl DataSink for DataSinkAzureBlobPlugin {
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
        info!("AzureBlob: uploaded {}", final_key);
        Ok(())
    }

    fn capability(&self) -> &'static skippr_runtime_sdk::plugins::cdc::SinkCapability {
        &skippr_runtime_sdk::plugins::cdc::sink_capabilities::AZURE_BLOB
    }
}

impl DataSinkAzureBlobPlugin {
    pub async fn new_with_config(
        _buffer_name: String,
        config: DataSinkAzureBlobPluginConfig,
    ) -> io::Result<Self> {
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

        let store = builder.build().map_err(|err| {
            io::Error::other(format!("Failed to build Azure blob store: {}", err))
        })?;

        Ok(Self {
            store: Box::new(store),
            config,
        })
    }
}
