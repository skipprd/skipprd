use crate::discover::Metadata;
use crate::helpers::configuration::{Config, Metrics};
use crate::helpers::offsets::{OffsetKey, Offsets};
use crate::helpers::Helpers;
use crate::ingest_work::{Ingest, IngestBatch, OUTPUT_FILES_STATIC};
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;
use tokio::time::Duration;

pub struct DataSourceStdinPlugin {
    ingest: Ingest,
    buffer_size: usize,
    buffer_timeout: Duration,
    buffer_threshold: Duration,
}

impl DataSourceStdinPlugin {
    pub async fn new() -> DataSourceStdinPlugin {
        DataSourceStdinPlugin {
            ingest: Ingest::new(),
            buffer_size: Config::getenv("DATA_SOURCE_BATCH_SIZE_BYTES", "1")
                .parse()
                .unwrap(),
            buffer_timeout: Duration::from_secs(
                Config::getenv("DATA_SOURCE_BATCH_SIZE_SECONDS", "1")
                    .parse()
                    .unwrap(),
            ),
            buffer_threshold: Duration::from_secs(
                Config::getenv("BUFFER_THRESHOLD_SECONDS", "5")
                    .parse()
                    .unwrap(),
            ),
        }
    }

    pub async fn sync(
        &mut self,
        metadata: Arc<Mutex<HashMap<String, Metadata>>>,
        metrics: Arc<Mutex<Metrics>>,
        offsets: Arc<Offsets>,
    ) {
        let (tx, rx): (Sender<Vec<u8>>, Receiver<Vec<u8>>) = mpsc::channel();

        let buffer_size = self.buffer_size;
        let buffer_timeout = self.buffer_timeout;

        thread::spawn(move || {
            let stdin = io::stdin();
            let reader = BufReader::new(stdin.lock());

            let mut buffer = Vec::new();
            let mut last_flush = Instant::now();

            for line_result in reader.lines() {
                match line_result {
                    Ok(line) => {
                        buffer.extend(line.into_bytes());

                        // Add newline after each line
                        buffer.push('\n' as u8);

                        if buffer.len() >= buffer_size || last_flush.elapsed() >= buffer_timeout {
                            if let Err(e) = tx.send(buffer.clone()) {
                                eprintln!("Error sending to buffer channel: {}", e);
                                break;
                            }
                            buffer.clear();
                            last_flush = Instant::now();
                        }
                    }
                    Err(e) => {
                        eprintln!("Error reading from stdin: {}", e);
                        break;
                    }
                }
            }

            // Send any remaining data
            if !buffer.is_empty() {
                if let Err(e) = tx.send(buffer) {
                    eprintln!("Error sending to buffer channel: {}", e);
                }
            }
        });

        loop {
            match rx.recv_timeout(self.buffer_threshold) {
                Ok(buffer) => {
                    let data = String::from_utf8_lossy(&buffer).to_string();
                    let batch = IngestBatch {
                        offset_key: OffsetKey {
                            namespace: "stdin".to_string(),
                            partition: Helpers::random_str(10), // no partition for stdin
                        },
                        data,
                    };

                    // Spawn a new task in the runtime for each batch received.
                    let metadata = Arc::clone(&metadata);
                    let metrics = Arc::clone(&metrics);
                    let offsets_clone = offsets.clone();

                    thread::spawn(move || {
                        Ingest::ingest_file(vec![batch], &metadata, &metrics, &offsets_clone)
                    })
                    .join()
                    .unwrap();
                }
                Err(e) => match e {
                    mpsc::RecvTimeoutError::Timeout => {
                        let mut output_files = OUTPUT_FILES_STATIC.lock().unwrap();
                        Ingest::flush_buffers(true, &mut output_files);
                    }
                    mpsc::RecvTimeoutError::Disconnected => {
                        eprintln!("Error receiving from buffer channel: {}", e);
                        break;
                    }
                },
            }
        }
    }
}
