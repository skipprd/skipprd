use std::fs;
use std::io::{BufRead, BufReader};
use std::fs::File;
use crate::buffer::BufferChunker;

pub struct DataSinkStdoutPlugin {
    buffer_name: String
}

impl DataSinkStdoutPlugin {
    pub async fn new(buffer_name: String) -> DataSinkStdoutPlugin {
        Self {
            buffer_name: buffer_name
        }
    }

    pub async fn sync(&self) {
        while let Some(filename) = BufferChunker::next_file(&self.buffer_name) {
            let file = File::open(&filename).expect("Could not open file");
            let reader = BufReader::new(file);

            for line in reader.lines() {
                match line {
                    Ok(output) => println!("{}", output),
                    Err(e) => println!("Error: {}", e),
                }
            }

            // If needed, you can delete the file after it's printed
            fs::remove_file(&filename).expect("Unable to remove file");
        }
    }
}
