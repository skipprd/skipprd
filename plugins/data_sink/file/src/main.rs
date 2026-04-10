mod cdc_encode;
mod parquet_util;

use std::fs;
use std::io;
use std::io::Write;
use std::path::Path;

use clap::Parser;
use datafusion::execution::SendableRecordBatchStream;
use serde_derive::{Deserialize, Serialize};
use skippr::buffer::BufferChunker;
use skippr::helpers::configuration::{Config, PIPELINE_NAME};
use skippr::helpers::logging::init_logging;
use skippr::ingest::partition_time::TimePartitioner;
use skippr::plugins::cdc;
use skippr::runtime_plugins::framing::{read_host_frame, write_plugin_frame};
use skippr::runtime_plugins::protocol::{
    HandshakeResponse, HostFrame, PluginFrame, RuntimePluginKind, SinkRunRequest,
    RUNTIME_PROTOCOL_VERSION,
};
use skippr::runtime_plugins::sdk::decode_record_batch_stream;
use tracing::error;

#[derive(Debug, Parser)]
struct FileRuntimePluginCli {}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct DataSinkFilePluginConfig {
    pub format: Option<String>,
    pub output_dir: Option<String>,
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("skippr-plugin-data-sink-file: {}", err);
        std::process::exit(1);
    }
}

async fn run() -> io::Result<()> {
    let _cli = FileRuntimePluginCli::parse();
    let log_level =
        std::env::var("SKIPPR_RUNTIME_LOG_LEVEL").unwrap_or_else(|_| "warn".to_string());
    init_logging(Some(log_level));

    let pipeline_name = std::env::var("PIPELINE_NAME").unwrap_or_else(|_| "default".to_string());
    {
        let mut current = PIPELINE_NAME.write();
        current.clear();
        current.push_str(&pipeline_name);
    }

    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();

    let handshake = match read_host_frame(&mut stdin).await? {
        HostFrame::Handshake(handshake) => handshake,
        other => {
            write_plugin_frame(
                &mut stdout,
                &PluginFrame::Error(format!(
                    "expected handshake as first frame, got {:?}",
                    other
                )),
            )
            .await?;
            return Err(io::Error::other(
                "runtime file sink did not receive a handshake",
            ));
        }
    };

    if handshake.protocol_version != RUNTIME_PROTOCOL_VERSION {
        write_plugin_frame(
            &mut stdout,
            &PluginFrame::Error(format!(
                "protocol version mismatch: host={} child={}",
                handshake.protocol_version, RUNTIME_PROTOCOL_VERSION
            )),
        )
        .await?;
        return Err(io::Error::other("runtime protocol version mismatch"));
    }

    let response = HandshakeResponse {
        protocol_version: RUNTIME_PROTOCOL_VERSION,
        kind: RuntimePluginKind::DataSink,
        plugin_name: "File".to_string(),
        source_capability: None,
        sink_capability: cdc::sink_capabilities::by_name("File").map(Into::into),
        supports_schema: false,
    };
    write_plugin_frame(&mut stdout, &PluginFrame::HandshakeAck(response)).await?;

    run_sink_loop(&mut stdin, &mut stdout).await
}

async fn run_sink_loop<R, W>(reader: &mut R, writer: &mut W) -> io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    loop {
        match read_host_frame(reader).await? {
            HostFrame::RunSink(request) => {
                run_sink_request(request).await?;
                write_plugin_frame(writer, &PluginFrame::SinkAck).await?;
            }
            HostFrame::Shutdown => return Ok(()),
            other => {
                write_plugin_frame(
                    writer,
                    &PluginFrame::Error(format!(
                        "unexpected sink frame after handshake: {:?}",
                        other
                    )),
                )
                .await?;
                return Err(io::Error::other("unexpected sink frame"));
            }
        }
    }
}

async fn run_sink_request(request: SinkRunRequest) -> io::Result<()> {
    request
        .config
        .0
        .expect_plugin("File")
        .map_err(io::Error::other)?;
    let config: DataSinkFilePluginConfig = request.config.0.decode().map_err(io::Error::other)?;
    let stream = decode_record_batch_stream(request.arrow_stream_bytes)?;
    let stream = match request.cdc_ctx.as_ref() {
        Some(ctx) => cdc_encode::augment_stream_with_cdc_columns(stream, &ctx.part_meta),
        None => stream,
    };
    sync_file_sink(&config, stream, request.filename).await
}

async fn sync_file_sink(
    config: &DataSinkFilePluginConfig,
    stream: SendableRecordBatchStream,
    filename: String,
) -> io::Result<()> {
    use skippr::metrics::counters;

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
    let data_dir = Config::get_data_dir();
    let output_file = Path::new(&format!(
        "{}/output_buffer/{}.parquet",
        data_dir, output_name
    ))
    .to_path_buf();
    let output_dir = output_file
        .parent()
        .ok_or_else(|| io::Error::other("output parquet path has no parent directory"))?;
    tokio::fs::create_dir_all(output_dir).await?;

    let parquet_bytes = parquet_util::serialize_to_parquet(stream)
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
