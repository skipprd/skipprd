use crate::buffer::BufferChunker;
use crate::helpers::configuration::Config;
use crate::helpers::logger::LogLevel;
use crate::helpers::Helpers;
use crate::{discover, flatten_metadata, METADATA};
use std::fs;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use nix::libc::{backtrace, mkdir};

pub struct DataOutputFilePlugin {
    output_dir: String,
    time_bucket: String,
    buffer_name: String,
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

    pub async fn sync(&self) {
        while let Some(filename) = BufferChunker::next_file(&self.buffer_name) {

            // get filename without path
            let new_filename = Path::new(&filename)
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();

            let output_file = Path::new(&self.output_dir)
                .join(&new_filename);

            match fs::copy(format!("./{}", &filename), &output_file) {
                Ok(_) => {
                    println!("Data copied to: {}", output_file.display());
                    match fs::remove_file(&filename) {
                        Ok(_) => {}
                        Err(_) => {
                            // @todo - log this back to skippr platform
                        }
                    }
                }
                Err(err) => {
                    println!(
                        "Failed to copy file: {}, will retry later. Error: {:?}",
                        filename,
                        err
                    );
                }
            }
        }
    }
}
