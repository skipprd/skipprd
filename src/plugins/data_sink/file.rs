use crate::buffer::BufferChunker;
use crate::helpers::configuration::{Config, OutputPluginConfig};

use std::fs;
use std::io::Write;

use crate::ingest::partition_time::TimePartitioner;
use crate::plugins::parquet_util::serialize_to_parquet;
use crate::plugins::DataSink;
use async_trait::async_trait;
use datafusion::execution::SendableRecordBatchStream;
use serde_derive::Deserialize;
use std::path::Path;
use tracing::error;

#[derive(Debug, Deserialize, Clone)]
pub struct DataOutputFilePluginConfig {
    pub format: Option<String>,
    pub output_dir: Option<String>,
}

impl From<OutputPluginConfig> for DataOutputFilePluginConfig {
    fn from(plugin_config: OutputPluginConfig) -> Self {
        match plugin_config {
            OutputPluginConfig::File(file_config) => file_config,
            _ => panic!("Invalid plugin type"),
        }
    }
}

pub struct DataOutputFilePlugin {
    #[allow(dead_code)]
    output_dir: String,
    #[allow(dead_code)]
    time_bucket: String,
    #[allow(dead_code)]
    buffer_name: String,
}

#[async_trait]
impl DataSink for DataOutputFilePlugin {
    async fn sync(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
    ) -> Result<(), std::io::Error> {
        self.inner_sync(stream, filename).await
    }
}

impl DataOutputFilePlugin {
    pub async fn new(buffer_name: String) -> DataOutputFilePlugin {
        let output_config = Config::get_pipeline_output_plugin_config()
            .ok()
            .and_then(|config| match config {
                OutputPluginConfig::File(file_config) => Some(file_config),
                _ => None,
            });
        Self::new_with_config(buffer_name, output_config).await
    }

    pub async fn new_with_config(
        buffer_name: String,
        output_config: Option<DataOutputFilePluginConfig>,
    ) -> DataOutputFilePlugin {
        let output_dir = output_config
            .as_ref()
            .and_then(|config| config.output_dir.clone())
            .unwrap_or_else(|| {
                let configured = Config::getenv("DATA_OUTPUT_FILE_DIR", "");
                if configured.is_empty() {
                    Config::getenv("DATA_OUTPUT_PATH", "")
                } else {
                    configured
                }
            });
        let time_bucket = Config::getenv("TRANSFORM_BATCH_TIME_UNIT", "");

        // Ensure the output_dir exists, creating parent directories if needed
        match fs::create_dir_all(&output_dir) {
            Ok(_) => {}
            Err(_) => {
                error!("Failed to create output directory: {}", output_dir);
            }
        }

        Self {
            output_dir,
            time_bucket,
            buffer_name: buffer_name,
        }
    }

    pub async fn inner_sync(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
    ) -> Result<(), std::io::Error> {
        use crate::metrics::counters;
        counters::inc_uploads_in_flight();

        let namespace = BufferChunker::decode_file_namespace(&filename);

        let mut full_key = "".to_string();
        if !namespace.is_empty() {
            full_key = format!("{}", namespace);
        }

        full_key = match BufferChunker::decode_file_partition(&filename).len() {
            0 => full_key,
            _ => format!(
                "{}/{}",
                full_key,
                BufferChunker::decode_file_partition(&filename)
            ),
        };

        let _key = match TimePartitioner::new(&filename).process() {
            Ok(k) => {
                full_key = format!("{}/{}", full_key, k);
            }
            Err(_e) => {}
        };

        let md5_digest = md5::compute(&filename);
        let md5_string = hex::encode(&md5_digest.0);

        // get filename without path
        // let new_filename = Path::new(&filename)
        //     .file_name()
        //     .unwrap()
        //     .to_str()
        //     .unwrap()
        //     .to_string();

        let new_filename = format!("{}/{}", full_key, md5_string);

        let data_dir = Config::get_data_dir();
        let output_dir = format!("{}/{}", data_dir, "output_buffer");
        let output_file = format!("{}/{}.parquet", output_dir, new_filename);

        let output_file = Path::new(&output_file);

        let output_dir = output_file.parent().unwrap();
        tokio::fs::create_dir_all(&output_dir).await?;

        let parquet_bytes = serialize_to_parquet(stream).await
            .map_err(|e| { counters::dec_uploads_in_flight(); e })?;

        let fp = fs::File::create(&output_file).map_err(|e| {
            counters::dec_uploads_in_flight();
            e
        })?;
        let mut buf_writer = std::io::BufWriter::new(fp);
        buf_writer.write_all(&parquet_bytes.bytes).map_err(|e| {
            counters::dec_uploads_in_flight();
            e
        })?;
        buf_writer.flush()?;

        counters::add_parquet_rows(parquet_bytes.meta_data.num_rows as u64);
        counters::add_parquet_bytes(parquet_bytes.size_bytes);
        counters::add_upload(1);
        counters::dec_uploads_in_flight();
        Ok(())
    }
}
