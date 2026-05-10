use std::io::{self, BufRead, BufReader};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_derive::Deserialize;
use tracing::error;

use crate::helpers::configuration::Config;
use skippr_runtime_sdk::progress::{OffsetKey, Offsets};
use crate::helpers::plugin_config::PluginConfigEntry;
use crate::helpers::Helpers;
use skippr_runtime_sdk::source_compat::{Ingest, IngestBatch, IngestTask, IngestTasks};
use skippr_runtime_sdk::plugins::{DataSink, DataSource, SourceExecutionContract, SourceOnceContract};
use crate::RUNNING;

#[derive(Debug, Deserialize, Clone)]
pub struct DataSourceStdinPluginConfig {
    pub mode: Option<String>,
    pub format: Option<String>,
    pub batch_size_bytes: Option<i64>,
    pub batch_size_seconds: Option<i64>,
}

impl TryFrom<PluginConfigEntry> for DataSourceStdinPluginConfig {
    type Error = String;

    fn try_from(plugin_config: PluginConfigEntry) -> Result<Self, Self::Error> {
        plugin_config.decode_for_plugin("Stdin")
    }
}

pub struct DataSourceStdinPlugin {
    ingest: Ingest,
    config: DataSourceStdinPluginConfig,
}

impl DataSourceStdinPlugin {
    pub async fn new() -> Self {
        let config: DataSourceStdinPluginConfig = match Config::get_pipeline_input_plugin_config() {
            Ok(c) => c.try_into().unwrap_or_else(|e| panic!("{}", e)),
            Err(_) => DataSourceStdinPluginConfig {
                mode: Some(Config::getenv("DATA_SOURCE_STDIN_MODE", "batch")),
                format: None,
                batch_size_bytes: Some(
                    Config::getenv("DATA_SOURCE_BATCH_SIZE_BYTES", "1")
                        .parse()
                        .unwrap_or(1),
                ),
                batch_size_seconds: Some(
                    Config::getenv("DATA_SOURCE_BATCH_SIZE_SECONDS", "1")
                        .parse()
                        .unwrap_or(1),
                ),
            },
        };

        Self {
            ingest: Ingest::new(),
            config,
        }
    }

    pub fn with_runtime_config(config: DataSourceStdinPluginConfig) -> Self {
        Self {
            ingest: Ingest::new(),
            config,
        }
    }

    fn dispatch_batch(
        &self,
        buffer: Vec<u8>,
        offsets: &Arc<Offsets>,
        shared_output: &Arc<Box<dyn DataSink + Send + Sync>>,
    ) {
        let data = String::from_utf8_lossy(&buffer).into_owned();
        let batch = IngestBatch {
            offset_key: OffsetKey {
                namespace: "stdin".to_string(),
                partition: Helpers::random_str(10),
            },
            data: data.clone(),
            bytes: data.len(),
            source_uri: String::new(),
            namespace: None,
            cdc_rows: None,
        };
        let mut tasks = IngestTasks::new();
        tasks.add(IngestTask::new(
            vec![batch],
            offsets.clone(),
            shared_output.clone(),
        ));
        self.ingest
            .ingest_file(&Arc::new(tasks), offsets, shared_output.clone());
    }
}

#[async_trait]
impl DataSource for DataSourceStdinPlugin {
    async fn sync(
        &mut self,
        offsets: Arc<Offsets>,
        shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
    ) -> Result<(), std::io::Error> {
        let (tx, rx) = mpsc::channel::<Vec<u8>>();

        let buffer_size = self.config.batch_size_bytes.unwrap_or(
            Config::getenv("DATA_SOURCE_BATCH_SIZE_BYTES", "1")
                .parse()
                .unwrap_or(1),
        ) as usize;
        let buffer_timeout = Duration::from_secs(
            self.config
                .batch_size_seconds
                .unwrap_or(
                    Config::getenv("DATA_SOURCE_BATCH_SIZE_SECONDS", "1")
                        .parse()
                        .unwrap_or(1),
                )
                .max(0) as u64,
        );

        thread::spawn(move || {
            let stdin = io::stdin();
            let reader = BufReader::new(stdin.lock());
            let mut buffer = Vec::new();
            let mut last_flush = Instant::now();

            for line_result in reader.lines() {
                match line_result {
                    Ok(line) => {
                        buffer.extend(line.into_bytes());
                        buffer.push(b'\n');

                        if buffer.len() >= buffer_size || last_flush.elapsed() >= buffer_timeout {
                            if let Err(e) = tx.send(buffer.clone()) {
                                error!("Error sending to buffer channel: {}", e);
                                break;
                            }
                            buffer.clear();
                            last_flush = Instant::now();
                        }
                    }
                    Err(e) => {
                        error!("Error reading from stdin: {}", e);
                        break;
                    }
                }
            }

            if !buffer.is_empty() {
                if let Err(e) = tx.send(buffer) {
                    error!("Error sending to buffer channel: {}", e);
                }
            }
        });

        let mode = self
            .config
            .mode
            .clone()
            .unwrap_or_else(|| Config::getenv("DATA_SOURCE_STDIN_MODE", "batch"));
        let stream_mode = mode.eq_ignore_ascii_case("stream");

        let poll = Duration::from_millis(500);

        if stream_mode {
            while RUNNING.read().load(Ordering::SeqCst) {
                match rx.recv_timeout(poll) {
                    Ok(buffer) => {
                        self.dispatch_batch(buffer, &offsets, &shared_output);
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        } else {
            loop {
                match rx.recv() {
                    Ok(buffer) => {
                        self.dispatch_batch(buffer, &offsets, &shared_output);
                    }
                    Err(_) => break,
                }
            }
        }

        Ok(())
    }

    fn execution_contract(&self) -> SourceExecutionContract {
        let stream_mode = self
            .config
            .mode
            .as_deref()
            .unwrap_or("batch")
            .eq_ignore_ascii_case("stream");
        if stream_mode {
            SourceExecutionContract::stream(SourceOnceContract::HostIdleBounded)
        } else {
            SourceExecutionContract::finite()
        }
    }
}
