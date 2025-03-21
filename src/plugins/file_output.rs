use crate::buffer::BufferChunker;
use crate::helpers::configuration::Config;



use std::{fs};
use std::io::Write;


use std::path::Path;
use async_trait::async_trait;
use datafusion::execution::SendableRecordBatchStream;
use crate::ingest::partition_time::TimePartitioner;
use crate::plugins::athena::DataOutputAwsAthenaPlugin;
use crate::plugins::DataOutputPlugin;


pub struct DataOutputFilePlugin {
    #[allow(dead_code)]
    output_dir: String,
    #[allow(dead_code)]
    time_bucket: String,
    #[allow(dead_code)]
    buffer_name: String,
}

#[async_trait]
impl DataOutputPlugin for DataOutputFilePlugin {
    async fn sync(&self, stream: SendableRecordBatchStream, filename: String) -> Result<(), std::io::Error> {
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

        let _key = match TimePartitioner::new(&filename).process() {
            Ok(k) => {
                full_key = format!("{}/{}", full_key, k);
            },
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

        let parquet_bytes = DataOutputAwsAthenaPlugin::serialize_to_parquet(stream).await?;

        let fp = fs::File::create(&output_file)?;
        let mut buf_writer = std::io::BufWriter::new(fp);
        buf_writer.write_all(&parquet_bytes.bytes)?;
        buf_writer.flush()?;

        // println!("Created output file: {}", output_file.display());

        Ok(())

    }
}
