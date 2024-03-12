use crate::buffer::BufferChunker;
use crate::helpers::configuration::Config;



use std::fs;
use std::io::Write;


use std::path::Path;
use std::sync::Arc;
use async_trait::async_trait;
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


        // get filename without path
        let new_filename = Path::new(&filename)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        let data_dir = Config::get_data_dir();
        let output_file = format!("{}/{}/{}.parquet", data_dir, "output_buffer", new_filename);
        let output_file = Path::new(&output_file);


        let parquet_bytes = DataOutputAwsAthenaPlugin::serialize_to_parquet(stream).await?;

        let mut fp = fs::File::create(&output_file)?;
        let mut buf_writer = std::io::BufWriter::new(fp);
        buf_writer.write_all(&parquet_bytes.bytes)?;
        buf_writer.flush()?;

        println!("Created output file: {}", output_file.display());

        Ok(())

    }
}
