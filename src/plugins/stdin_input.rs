use std::io::{self, BufRead, BufReader};
use std::sync::mpsc::{self, Sender, Receiver};
use std::thread;
use std::time::Instant;
use tokio::time::Duration;
use crate::helpers::configuration::{Config, Metrics};
use crate::ingest_work::{Ingest, IngestBatch, OUTPUT_FILES_STATIC};
use std::collections::HashMap;
use crate::discover::Metadata;
use std::sync::{Arc, Mutex};
use crate::helpers::Helpers;
use crate::helpers::offsets::{OffsetKey, Offsets};

pub struct DataSourceStdinPlugin {
    ingest: Ingest,
    buffer_size: usize,
    buffer_timeout: Duration,
}

impl DataSourceStdinPlugin {
    pub async fn new() -> DataSourceStdinPlugin {
        DataSourceStdinPlugin {
            ingest: Ingest::new(),
            buffer_size: Config::getenv("DATA_SOURCE_BATCH_SIZE_BYTES", "1").parse().unwrap(),
            buffer_timeout: Duration::from_secs(Config::getenv("DATA_SOURCE_BATCH_SIZE_SECONDS", "1").parse().unwrap()),
        }
    }

    pub async fn sync(
        &mut self,
        metadata: Arc<Mutex<HashMap<String, Metadata>>>,
        metrics: Arc<Mutex<Metrics>>,
    ) {
        let (tx, rx): (Sender<Vec<u8>>, Receiver<Vec<u8>>) = mpsc::channel();

        thread::spawn(move || {
            let stdin = io::stdin();
            let reader = BufReader::new(stdin.lock());

            for line_result in reader.lines() {
                match line_result {
                    Ok(line) => {
                        let bytes = line.into_bytes();
                        if let Err(e) = tx.send(bytes) {
                            eprintln!("Error sending to buffer channel: {}", e);
                            break;
                        }
                    }
                    Err(e) => {
                        eprintln!("Error reading from stdin: {}", e);
                        break;
                    }
                }
            }
        });

        loop {
            match rx.recv() {
                Ok(buffer) => {
                    let data = String::from_utf8_lossy(&buffer).to_string();
                    let batch = IngestBatch {
                        offset_key: OffsetKey {
                            namespace: "stdin".to_string(),
                            partition: Helpers::random_str(10),  // no partition for stdin
                        },
                        data,
                    };

                    // Spawn a new task in the runtime for each batch received.
                    let metadata = Arc::clone(&metadata);
                    let metrics = Arc::clone(&metrics);
                    thread::spawn(move || {
                        Ingest::ingest_file(vec![batch], &metadata, &metrics, &Arc::new(Offsets::init().unwrap()));
                    }).join().unwrap();
                    let mut output_files = OUTPUT_FILES_STATIC.lock().unwrap();
                    Ingest::flush_buffers(true, &mut output_files);
                }
                Err(e) => {
                    eprintln!("Error receiving from buffer channel: {}", e);
                    break;
                }
            }
        }
    }
}
