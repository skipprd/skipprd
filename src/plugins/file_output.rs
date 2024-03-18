use crate::buffer::BufferChunker;
use crate::helpers::configuration::Config;



use std::{fs, io};
use std::io::Write;


use std::path::Path;
use std::sync::Arc;
use async_trait::async_trait;
use chrono::{Datelike, DateTime, Timelike};
use datafusion::execution::SendableRecordBatchStream;
use parquet::arrow::ArrowWriter;
use crate::helpers::offsets::Offsets;
use crate::helpers::timed_rwlock::TimedRwLock;
use crate::plugins::athena::DataOutputAwsAthenaPlugin;
use crate::plugins::DataOutputPlugin;
use crate::plugins::file_input::DataSourceLocalFilePlugin;


pub struct DataOutputFilePlugin {
    output_dir: String,
    time_bucket: String,
    buffer_name: String,
}

#[async_trait]
impl DataOutputPlugin for DataOutputFilePlugin {
    async fn sync(&mut self, stream: SendableRecordBatchStream, filename: String) -> Result<(), std::io::Error> {
        self.inner_sync(stream, filename).await
    }
}


impl DataOutputFilePlugin {
    pub async fn new(buffer_name: String) -> DataOutputFilePlugin {
        let output_dir = Config::getenv("DATA_OUTPUT_FILE_DIR", "");
        let time_bucket = Config::getenv("TRANSFORM_BATCH_TIME_UNIT", "");

        // Ensure the output_dir exists, creating parent directories if needed
        match fs::create_dir_all(&output_dir) {
            Ok(_) => {}
            Err(_) => {
                println!("Failed to create output directory: {}", output_dir);
            }
        }

        Self {
            output_dir,
            time_bucket,
            buffer_name: buffer_name
        }
    }

    pub async fn inner_sync(&self, stream: SendableRecordBatchStream, filename: String) -> Result<(), std::io::Error> {
        // while let Some(filename) = BufferChunker::next_file(&self.buffer_name) {

        let namespace = BufferChunker::decode_file_namespace(&filename);

        let mut full_key = "".to_string();
        if !namespace.is_empty() {
            full_key = format!("{}", namespace);
        }

        full_key = match BufferChunker::decode_file_partition(&filename).len() {
            0 => full_key,
            _ => format!("{}/{}", full_key, BufferChunker::decode_file_partition(&filename)),
        };

        let time_partition_str = BufferChunker::decode_file_time_to_datetime_string(&filename);

        if !time_partition_str.is_empty() {
            let granularity_target = Config::get_transform_batch_time_unit();

            let date = match DateTime::parse_from_rfc3339(&time_partition_str) {
                Ok(date) => date,
                Err(err) => {
                    println!(
                        "Failed to parse time partition string {}, Error: {}",
                        time_partition_str,
                        err.to_string()
                    );
                    // continue;
                    return Err(io::Error::new(io::ErrorKind::Other, "Failed to parse time partition string"));
                }
            };

            for granularity in crate::plugins::athena::GRANULARITIES.iter() {
                let foo: u32 = match granularity {
                    &"year" => date.year() as u32,
                    &"month" => date.month(),
                    &"day" => date.day(),
                    &"hour" => date.hour(),
                    &"minute" => date.minute(),
                    _ => {
                        panic!("Did not recognise date granularity of {}", granularity);
                    }
                };

                full_key = format!("{}/{}={}", full_key, granularity, foo);

                if granularity == &granularity_target {
                    break;
                }
            }
        }

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

        let parquet_bytes = DataOutputAwsAthenaPlugin::serialize_to_parquet(stream).await?;

        let fp = fs::File::create(&output_file)?;
        let mut buf_writer = std::io::BufWriter::new(fp);
        buf_writer.write_all(&parquet_bytes.bytes)?;
        buf_writer.flush()?;

        // println!("Created output file: {}", output_file.display());

        Ok(())

    }
}
