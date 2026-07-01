mod cdc_encode;
mod parquet_util;

use std::fs;
use std::io;
use std::io::Write;
use std::path::Path;

use async_trait::async_trait;
use clap::Parser;
use datafusion::execution::SendableRecordBatchStream;
use serde_derive::{Deserialize, Serialize};
use skippr_runtime_sdk::plugins::cdc;
use skippr_runtime_sdk::plugins::DataSink;
use skippr_runtime_sdk::plugins::{SinkWriteContext, SinkWriteOutcome};
use skippr_runtime_sdk::sink_compat::partition_time::TimePartitioner;
use skippr_runtime_sdk::sink_compat::BufferChunker;
use skippr_runtime_sdk::sink_idempotency::{manifest_object_name, ObjectWriteManifest};
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
}

skippr_runtime_sdk::declare_sink_spec!(
    FileSinkSpec,
    FileSinkRuntimePlugin,
    skippr_runtime_sdk::plugins::cdc::sink_capabilities::FILE,
    skippr_runtime_sdk::plugins::DeterministicObjectOverwrite
);

#[async_trait]
impl DataSink for FileSinkRuntimePlugin {
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
            stream,
            ctx.filename,
            object_stem.as_deref(),
        )
        .await
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
            if ObjectWriteManifest::from_json_bytes(&bytes)? == expected_manifest {
                return Ok(SinkWriteOutcome::AlreadyApplied);
            }
        }
        sync_file_sink(
            &self.config,
            &self.data_dir,
            &self.order_fields,
            stream,
            ctx.filename.clone(),
            Some(&object_stem),
        )
        .await?;
        fs::write(manifest_file, expected_manifest.to_json_bytes()?)?;
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

#[tokio::main]
async fn main() {
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
            })
        },
    )
    .await
    {
        eprintln!("skippr-plugin-data-sink-file: {}", err);
        std::process::exit(1);
    }
}

async fn sync_file_sink(
    config: &DataSinkFilePluginConfig,
    data_dir: &str,
    order_fields: &[String],
    stream: SendableRecordBatchStream,
    filename: String,
    object_stem: Option<&str>,
) -> io::Result<()> {
    use skippr_runtime_sdk::metrics::counters;

    if let Some(output_dir) = config.output_dir.as_deref() {
        if let Err(err) = fs::create_dir_all(output_dir) {
            error!(
                "Failed to create configured output directory {}: {}",
                output_dir, err
            );
        }
    }

    counters::inc_uploads_in_flight();

    let object_stem = object_stem
        .map(str::to_string)
        .unwrap_or_else(|| hex::encode(md5::compute(&filename).0));
    let output_file = output_file_path(data_dir, &filename, &object_stem)?;
    let output_dir = output_file
        .parent()
        .ok_or_else(|| io::Error::other("output parquet path has no parent directory"))?;
    tokio::fs::create_dir_all(output_dir).await?;

    let parquet_bytes = parquet_util::serialize_to_parquet_with_order_fields(stream, order_fields)
        .await
        .map_err(|err| {
            counters::dec_uploads_in_flight();
            err
        })?;

    let file = fs::File::create(&output_file).map_err(|err| {
        counters::dec_uploads_in_flight();
        err
    })?;
    let mut buf_writer = std::io::BufWriter::new(file);
    buf_writer.write_all(&parquet_bytes.bytes).map_err(|err| {
        counters::dec_uploads_in_flight();
        err
    })?;
    buf_writer.flush().map_err(|err| {
        counters::dec_uploads_in_flight();
        err
    })?;

    counters::add_parquet_rows(parquet_bytes.num_rows as u64);
    counters::add_parquet_bytes(parquet_bytes.size_bytes);
    counters::add_upload(1);
    counters::dec_uploads_in_flight();
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
    Ok(Path::new(&format!(
        "{}/output_buffer/{}.parquet",
        data_dir, output_name
    ))
    .to_path_buf())
}
