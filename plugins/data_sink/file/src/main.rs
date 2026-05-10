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
use skippr_runtime_sdk::sink_compat::BufferChunker;
use skippr_runtime_sdk::sink_compat::partition_time::TimePartitioner;
use skippr_runtime_sdk::plugins::cdc;
use skippr_runtime_sdk::plugins::DataSink;
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

#[async_trait]
impl DataSink for FileSinkRuntimePlugin {
    async fn sync(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
        cdc_ctx: Option<&cdc::SyncContext>,
    ) -> Result<(), io::Error> {
        let stream = match cdc_ctx {
            Some(ctx) => cdc_encode::augment_stream_with_cdc_columns(stream, &ctx.part_meta),
            None => stream,
        };
        sync_file_sink(
            &self.config,
            &self.data_dir,
            &self.order_fields,
            stream,
            filename,
        )
        .await
    }

    async fn install_schema_state(
        &self,
        _schema_version: u64,
        _namespaces: &std::collections::BTreeMap<String, skippr_runtime_sdk::discover::OutputMetadata>,
    ) -> Result<(), io::Error> {
        Ok(())
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

    let namespace = BufferChunker::decode_file_namespace(&filename);
    let mut full_key = if namespace.is_empty() {
        String::new()
    } else {
        namespace
    };

    let partition = BufferChunker::decode_file_partition(&filename);
    if !partition.is_empty() {
        full_key = format!("{}/{}", full_key, partition);
    }

    if let Ok(time_key) = TimePartitioner::new(&filename).process() {
        full_key = format!("{}/{}", full_key, time_key);
    }

    let md5_digest = md5::compute(&filename);
    let output_name = format!("{}/{}", full_key, hex::encode(md5_digest.0));
    let output_file = Path::new(&format!(
        "{}/output_buffer/{}.parquet",
        data_dir, output_name
    ))
    .to_path_buf();
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

    counters::add_parquet_rows(parquet_bytes.meta_data.num_rows as u64);
    counters::add_parquet_bytes(parquet_bytes.size_bytes);
    counters::add_upload(1);
    counters::dec_uploads_in_flight();
    Ok(())
}
