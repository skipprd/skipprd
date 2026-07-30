#[path = "../../../shared/cdc_encode.rs"]
mod cdc_encode;

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use clap::Parser;
use datafusion::execution::SendableRecordBatchStream;
use futures::StreamExt;
use serde_derive::{Deserialize, Serialize};
use skippr_object_writer::backends::AtomicFileBackend;
use skippr_object_writer::{
    ObjectWriteError, ObjectWriteReceipt, ObjectWriteRequest, ObjectWriteSession,
    ObjectWriterConfig,
};
use skippr_runtime_sdk::plugins::cdc;
use skippr_runtime_sdk::plugins::DataSink;
use skippr_runtime_sdk::plugins::{SinkPreflightOutcome, SinkWriteContext, SinkWriteOutcome};
use skippr_runtime_sdk::sink_compat::partition_time::TimePartitioner;
use skippr_runtime_sdk::sink_compat::BufferChunker;
use skippr_runtime_sdk::sink_idempotency::{
    legacy_chunk_idempotency_key, manifest_object_name, persisted_object_write_matches,
    GroupedWriteReceipt, ObjectWriteManifest,
};
use skippr_runtime_sdk::sink_runtime_entry::run_runtime_data_sink_plugin;
use tracing::error;

#[derive(Debug, Parser)]
struct FileRuntimePluginCli {}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct DataSinkFilePluginConfig {
    pub format: Option<String>,
    pub output_dir: Option<String>,
}

struct FileSinkRuntimePlugin {
    config: DataSinkFilePluginConfig,
    data_dir: String,
    order_fields: Vec<String>,
    object_backend: Arc<AtomicFileBackend>,
}

skippr_runtime_sdk::declare_sink_spec!(
    FileSinkSpec,
    FileSinkRuntimePlugin,
    skippr_runtime_sdk::plugins::cdc::sink_capabilities::FILE,
    skippr_runtime_sdk::plugins::DeterministicObjectOverwrite
);

#[async_trait]
impl DataSink for FileSinkRuntimePlugin {
    async fn preflight(
        &self,
        ctx: SinkWriteContext<'_>,
    ) -> Result<SinkPreflightOutcome, io::Error> {
        if !ctx.is_grouped() {
            return Ok(SinkPreflightOutcome::Ready);
        }
        ctx.validate_grouped::<skippr_runtime_sdk::plugins::DeterministicObjectOverwrite>()
            .map_err(|err| io::Error::new(io::ErrorKind::Unsupported, err))?;
        let object_stem = skippr_runtime_sdk::sink_idempotency::deterministic_object_name(
            &ctx.idempotency_key,
            "",
        )?;
        let output_file = output_file_path(&self.data_dir, &ctx.filename, &object_stem)?;
        let manifest_file = output_file.with_file_name(manifest_object_name(
            output_file
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("output.parquet"),
        ));
        let expected_manifest = ObjectWriteManifest::from_context(
            ctx.compaction_id,
            ctx.idempotency_key,
            ctx.schema_fingerprint,
            &ctx.wal_refs,
        );
        if manifest_file.exists()
            && ObjectWriteManifest::from_json_bytes(&fs::read(&manifest_file)?)?
                .matches_manifest(&expected_manifest)
        {
            return Ok(SinkPreflightOutcome::AlreadyApplied {
                authority: manifest_file.to_string_lossy().into_owned(),
            });
        }
        Ok(SinkPreflightOutcome::Ready)
    }

    async fn sync(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
        cdc_ctx: Option<&cdc::SyncContext>,
    ) -> Result<(), io::Error> {
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
    ) -> Result<(), io::Error> {
        ctx.validate_grouped::<skippr_runtime_sdk::plugins::DeterministicObjectOverwrite>()
            .map_err(|err| io::Error::new(io::ErrorKind::Unsupported, err))?;
        let stream = match ctx.cdc_ctx {
            Some(cdc) => cdc_encode::augment_stream_with_cdc_columns(stream, &cdc.part_meta),
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
        sync_file_sink(
            &self.config,
            &self.data_dir,
            &self.order_fields,
            self.object_backend.clone(),
            stream,
            ctx.filename,
            object_stem.as_deref(),
        )
        .await
        .map(|_| ())
    }

    async fn sync_with_context_result(
        &self,
        stream: SendableRecordBatchStream,
        ctx: SinkWriteContext<'_>,
    ) -> Result<SinkWriteOutcome, io::Error> {
        if !ctx.is_grouped() {
            self.sync_with_context(stream, ctx).await?;
            return Ok(SinkWriteOutcome::Applied);
        }
        ctx.validate_grouped::<skippr_runtime_sdk::plugins::DeterministicObjectOverwrite>()
            .map_err(|err| io::Error::new(io::ErrorKind::Unsupported, err))?;
        let object_stem = skippr_runtime_sdk::sink_idempotency::deterministic_object_name(
            &ctx.idempotency_key,
            "",
        )?;
        let output_file = output_file_path(&self.data_dir, &ctx.filename, &object_stem)?;
        let manifest_file = output_file.with_file_name(manifest_object_name(
            output_file
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("output.parquet"),
        ));
        let expected_manifest = ObjectWriteManifest::from_context(
            ctx.compaction_id.clone(),
            ctx.idempotency_key.clone(),
            ctx.schema_fingerprint.clone(),
            &ctx.wal_refs,
        );
        if manifest_file.exists() {
            let bytes = fs::read(&manifest_file)?;
            if persisted_object_write_matches(&bytes, &expected_manifest)? {
                return Ok(SinkWriteOutcome::AlreadyApplied);
            }
        }
        let stream = match ctx.cdc_ctx {
            Some(cdc) => cdc_encode::augment_stream_with_cdc_columns(stream, &cdc.part_meta),
            None => stream,
        };
        let receipt = sync_file_sink(
            &self.config,
            &self.data_dir,
            &self.order_fields,
            self.object_backend.clone(),
            stream,
            ctx.filename.clone(),
            Some(&object_stem),
        )
        .await?;
        write_local_receipt(&manifest_file, &expected_manifest, &receipt).await?;
        Ok(SinkWriteOutcome::Applied)
    }

    async fn sync_grouped(
        &self,
        mut reader: skippr_runtime_sdk::plugins::GroupedBatchReader,
        ctx: skippr_runtime_sdk::plugins::GroupedSinkWriteContext<'_>,
    ) -> Result<SinkWriteOutcome, io::Error> {
        let object_stem = skippr_runtime_sdk::sink_idempotency::deterministic_object_name(
            &ctx.idempotency_key,
            "",
        )?;
        let output_file = output_file_path(&self.data_dir, &ctx.filename, &object_stem)?;
        let manifest_file = output_file.with_file_name(manifest_object_name(
            output_file
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("output.parquet"),
        ));
        let expected_manifest = ObjectWriteManifest::from_context(
            ctx.compaction_id.clone(),
            ctx.idempotency_key.clone(),
            ctx.schema_fingerprint.clone(),
            ctx.wal_refs.as_slice(),
        );
        if manifest_file.exists() {
            let bytes = fs::read(&manifest_file)?;
            if persisted_object_write_matches(&bytes, &expected_manifest)? {
                return Ok(SinkWriteOutcome::AlreadyApplied);
            }
        }
        let (legacy_output_file, legacy_manifest_file, legacy_manifest) =
            legacy_file_chunk_manifest(&self.data_dir, &ctx, 0)?;
        if local_manifest_matches(&legacy_manifest_file, &legacy_manifest)?
            || legacy_output_file.exists()
        {
            let receipt = sync_file_legacy_chunks(
                &self.config,
                &self.data_dir,
                &self.order_fields,
                self.object_backend.clone(),
                &mut reader,
                &ctx,
            )
            .await?;
            write_local_receipt(&manifest_file, &expected_manifest, &receipt).await?;
            return Ok(SinkWriteOutcome::Applied);
        }
        let (stream, _stream_progress) = reader.into_stream();
        let stream = match ctx.cdc_ctx {
            Some(cdc) => cdc_encode::augment_stream_with_cdc_columns(stream, &cdc.part_meta),
            None => stream,
        };
        let receipt = sync_file_sink(
            &self.config,
            &self.data_dir,
            &self.order_fields,
            self.object_backend.clone(),
            stream,
            ctx.filename,
            Some(&object_stem),
        )
        .await?;
        write_local_receipt(&manifest_file, &expected_manifest, &receipt).await?;
        Ok(SinkWriteOutcome::Applied)
    }

    async fn install_schema_state(
        &self,
        _schema_version: u64,
        _namespaces: &std::collections::BTreeMap<
            String,
            skippr_runtime_sdk::discover::OutputMetadata,
        >,
    ) -> Result<(), io::Error> {
        Ok(())
    }

    fn capability(&self) -> &'static skippr_runtime_sdk::plugins::cdc::SinkCapability {
        &skippr_runtime_sdk::plugins::cdc::sink_capabilities::FILE
    }
}

skippr_runtime_sdk::runtime_main!(async {
    let _cli = FileRuntimePluginCli::parse();
    if let Err(err) = run_runtime_data_sink_plugin(
        "File",
        "File",
        skippr_runtime_sdk::plugins::cdc::sink_capabilities::by_name("File").map(Into::into),
        false,
        "skippr-plugin-data-sink-file",
        |install| async move {
            let config: DataSinkFilePluginConfig =
                install.config.0.decode().map_err(io::Error::other)?;
            Ok(FileSinkRuntimePlugin {
                config,
                data_dir: install.context.data_dir,
                order_fields: install.context.output_layout.order_fields,
                object_backend: Arc::new(AtomicFileBackend::new()),
            })
        },
    )
    .await
    {
        eprintln!("skippr-plugin-data-sink-file: {}", err);
        std::process::exit(1);
    }
});

async fn sync_file_sink(
    config: &DataSinkFilePluginConfig,
    data_dir: &str,
    order_fields: &[String],
    object_backend: Arc<AtomicFileBackend>,
    stream: SendableRecordBatchStream,
    filename: String,
    object_stem: Option<&str>,
) -> io::Result<ObjectWriteReceipt> {
    use skippr_runtime_sdk::metrics::counters;

    if let Some(output_dir) = config.output_dir.as_deref() {
        if let Err(err) = fs::create_dir_all(output_dir) {
            error!(
                "Failed to create configured output directory {}: {}",
                output_dir, err
            );
        }
    }

    let object_stem = object_stem
        .map(str::to_string)
        .unwrap_or_else(|| hex::encode(md5::compute(&filename).0));
    let output_file = output_file_path(data_dir, &filename, &object_stem)?;
    let schema = stream.schema();
    let effective_order =
        skippr_runtime_sdk::converters::parquet_ordering::resolve_effective_order_from_fields(
            &schema,
            order_fields,
        );
    let writer_properties =
        skippr_runtime_sdk::converters::parquet_ordering::build_writer_properties(
            &schema,
            &effective_order,
            skippr_runtime_sdk::converters::parquet_ordering::default_streaming_row_group_size(),
        );
    let batches = stream.map(move |batch| {
        let batch = batch.map_err(ObjectWriteError::input)?;
        skippr_runtime_sdk::converters::parquet_ordering::sort_batch(&batch, &effective_order)
            .map_err(ObjectWriteError::input)
    });
    let session = ObjectWriteSession::new(
        object_backend,
        ObjectWriteRequest::new(output_file.to_string_lossy()),
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

fn legacy_file_chunk_manifest(
    data_dir: &str,
    ctx: &skippr_runtime_sdk::plugins::GroupedSinkWriteContext<'_>,
    chunk_index: u64,
) -> io::Result<(PathBuf, PathBuf, ObjectWriteManifest)> {
    let object_stem = legacy_chunk_idempotency_key(&ctx.idempotency_key, chunk_index);
    let chunk_filename = ctx.chunk_filename(chunk_index, false);
    let output_file = output_file_path(data_dir, &chunk_filename, &object_stem)?;
    let manifest_file = output_file.with_file_name(manifest_object_name(
        output_file
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("output.parquet"),
    ));
    let manifest = ObjectWriteManifest::from_context(
        legacy_chunk_idempotency_key(&ctx.compaction_id, chunk_index),
        object_stem,
        ctx.schema_fingerprint.clone(),
        ctx.wal_refs.as_slice(),
    );
    Ok((output_file, manifest_file, manifest))
}

fn local_manifest_matches(path: &Path, expected: &ObjectWriteManifest) -> io::Result<bool> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    persisted_object_write_matches(&bytes, expected)
}

async fn sync_file_legacy_chunks(
    config: &DataSinkFilePluginConfig,
    data_dir: &str,
    order_fields: &[String],
    object_backend: Arc<AtomicFileBackend>,
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
        let chunk_index = chunk.chunk_index;
        let chunk_rows = chunk.rows;
        let object_stem = legacy_chunk_idempotency_key(&ctx.idempotency_key, chunk_index);
        let chunk_filename = ctx.chunk_filename(chunk_index, false);
        let (output_file, manifest_file, expected) =
            legacy_file_chunk_manifest(data_dir, ctx, chunk_index)?;
        let (chunk_bytes, chunk_transport_count, chunk_etag) =
            if local_manifest_matches(&manifest_file, &expected)? {
                let metadata = fs::metadata(&output_file)?;
                (metadata.len(), 1, Some(format!("local-{}", metadata.len())))
            } else {
                let chunk_cdc = ctx.chunk_cdc_context(&chunk)?;
                let stream = chunk.into_stream(schema.clone());
                let stream = match chunk_cdc.as_ref() {
                    Some(cdc) => {
                        cdc_encode::augment_stream_with_cdc_columns(stream, &cdc.part_meta)
                    }
                    None => stream,
                };
                let receipt = sync_file_sink(
                    config,
                    data_dir,
                    order_fields,
                    object_backend.clone(),
                    stream,
                    chunk_filename,
                    Some(&object_stem),
                )
                .await?;
                (receipt.bytes, receipt.transport_chunk_count, receipt.etag)
            };
        object_key.get_or_insert_with(|| output_file.to_string_lossy().into_owned());
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
        object_key: object_key.ok_or_else(|| io::Error::other("No rows to write to parquet"))?,
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

async fn write_local_receipt(
    manifest_file: &Path,
    manifest: &ObjectWriteManifest,
    object_receipt: &ObjectWriteReceipt,
) -> io::Result<()> {
    let receipt = GroupedWriteReceipt::from_manifest_and_upload(
        manifest,
        format!("file://{}", object_receipt.object_key),
        object_receipt.etag.clone().unwrap_or_default(),
        object_receipt.checksum.clone(),
        object_receipt.rows,
        object_receipt.bytes,
        object_receipt.transport_chunk_count,
    );
    let temporary = manifest_file.with_extension("json.tmp");
    tokio::fs::write(&temporary, receipt.to_json_bytes()?).await?;
    tokio::fs::rename(&temporary, manifest_file).await?;
    Ok(())
}

fn output_file_path(
    data_dir: &str,
    filename: &str,
    object_stem: &str,
) -> io::Result<std::path::PathBuf> {
    let namespace = BufferChunker::decode_file_namespace(filename);
    let mut full_key = if namespace.is_empty() {
        String::new()
    } else {
        namespace
    };

    let partition = BufferChunker::decode_file_partition(filename);
    if !partition.is_empty() {
        full_key = format!("{}/{}", full_key, partition);
    }

    let filename_owned = filename.to_string();
    if let Ok(time_key) = TimePartitioner::new(&filename_owned).process() {
        full_key = format!("{}/{}", full_key, time_key);
    }

    let output_name = format!("{}/{}", full_key, object_stem);
    Ok(Path::new(&format!("{}/output/{}.parquet", data_dir, output_name)).to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_output_path_is_unchanged() {
        assert_eq!(
            output_file_path("/data", "namespace=events", "apply-0001").unwrap(),
            PathBuf::from("/data/output/events/apply-0001.parquet")
        );
    }
}
