use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;
use serde_derive::Deserialize;
use tokio::io::AsyncBufReadExt;
use tokio::net::{TcpListener, UdpSocket};
use tokio::time::Duration;
use tracing::{error, info};

use crate::helpers::configuration::Config;
use crate::helpers::offsets::{OffsetKey, Offsets};
use crate::helpers::plugin_config::PluginConfigEntry;
use crate::ingest_work::{Ingest, IngestBatch, IngestTask, IngestTasks};
use crate::plugins::{DataSink, DataSource};
use crate::RUNNING;

#[derive(Debug, Deserialize, Clone)]
pub struct DataSourceSocketPluginConfig {
    pub mode: String,
    pub address: String,
    pub framing: Option<String>,
    pub format: Option<String>,
    pub batch_size_bytes: Option<i64>,
    pub batch_size_seconds: Option<i64>,
}

impl TryFrom<PluginConfigEntry> for DataSourceSocketPluginConfig {
    type Error = String;

    fn try_from(plugin_config: PluginConfigEntry) -> Result<Self, Self::Error> {
        plugin_config.decode_for_plugin("Socket")
    }
}

pub struct DataSourceSocketPlugin {
    ingest: Ingest,
    config: DataSourceSocketPluginConfig,
}

impl DataSourceSocketPlugin {
    pub async fn new() -> Self {
        let config: DataSourceSocketPluginConfig = match Config::get_pipeline_input_plugin_config()
        {
            Ok(c) => c.try_into().unwrap_or_else(|e| panic!("{}", e)),
            Err(_) => DataSourceSocketPluginConfig {
                mode: Config::getenv("SOCKET_MODE", "tcp"),
                address: Config::getenv("SOCKET_ADDRESS", "0.0.0.0:9000"),
                framing: None,
                format: None,
                batch_size_bytes: None,
                batch_size_seconds: None,
            },
        };
        Self {
            ingest: Ingest::new(),
            config,
        }
    }

    pub fn with_runtime_config(config: DataSourceSocketPluginConfig) -> Self {
        Self {
            ingest: Ingest::new(),
            config,
        }
    }

    fn ingest_line(
        &self,
        data: String,
        counter: &mut u64,
        offsets: &Arc<Offsets>,
        shared_output: &Arc<Box<dyn DataSink + Send + Sync>>,
    ) {
        if data.is_empty() {
            return;
        }
        *counter += 1;
        let bytes = data.len();
        let namespace = format!("socket.{}.{}", self.config.mode, self.config.address);
        let offset_key = OffsetKey {
            namespace: namespace.clone(),
            partition: counter.to_string(),
        };
        let mut ingest_tasks = IngestTasks::new();
        ingest_tasks.add(IngestTask::new(
            vec![IngestBatch {
                offset_key,
                data,
                bytes,
                source_uri: format!("socket://{}:{}", self.config.mode, self.config.address),
                namespace: Some(namespace),
                cdc_rows: None,
            }],
            offsets.clone(),
            shared_output.clone(),
        ));
        self.ingest
            .ingest_file(&Arc::new(ingest_tasks), offsets, shared_output.clone());
    }
}

#[async_trait]
impl DataSource for DataSourceSocketPlugin {
    async fn sync(
        &mut self,
        offsets: Arc<Offsets>,
        shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
    ) -> Result<(), std::io::Error> {
        let mut counter: u64 = 0;

        match self.config.mode.as_str() {
            "tcp" => {
                let listener = TcpListener::bind(&self.config.address).await?;
                info!("Socket (TCP): listening on {}", self.config.address);
                while RUNNING.read().load(Ordering::SeqCst) {
                    match tokio::time::timeout(Duration::from_secs(1), listener.accept()).await {
                        Ok(Ok((stream, _addr))) => {
                            let reader = tokio::io::BufReader::new(stream);
                            let mut lines = reader.lines();
                            while let Ok(Some(line)) = lines.next_line().await {
                                self.ingest_line(line, &mut counter, &offsets, &shared_output);
                                if !RUNNING.read().load(Ordering::SeqCst) {
                                    break;
                                }
                            }
                        }
                        Ok(Err(e)) => error!("TCP accept error: {}", e),
                        Err(_) => continue,
                    }
                }
            }
            "udp" => {
                let socket = UdpSocket::bind(&self.config.address).await?;
                info!("Socket (UDP): listening on {}", self.config.address);
                let mut buf = [0u8; 65535];
                while RUNNING.read().load(Ordering::SeqCst) {
                    match tokio::time::timeout(Duration::from_secs(1), socket.recv_from(&mut buf))
                        .await
                    {
                        Ok(Ok((len, _addr))) => {
                            let data = String::from_utf8_lossy(&buf[..len]).into_owned();
                            for line in data.lines() {
                                self.ingest_line(
                                    line.to_string(),
                                    &mut counter,
                                    &offsets,
                                    &shared_output,
                                );
                            }
                        }
                        Ok(Err(e)) => error!("UDP recv error: {}", e),
                        Err(_) => continue,
                    }
                }
            }
            #[cfg(unix)]
            "unix" => {
                let listener = tokio::net::UnixListener::bind(&self.config.address)?;
                info!("Socket (Unix): listening on {}", self.config.address);
                while RUNNING.read().load(Ordering::SeqCst) {
                    match tokio::time::timeout(Duration::from_secs(1), listener.accept()).await {
                        Ok(Ok((stream, _addr))) => {
                            let reader = tokio::io::BufReader::new(stream);
                            let mut lines = reader.lines();
                            while let Ok(Some(line)) = lines.next_line().await {
                                self.ingest_line(line, &mut counter, &offsets, &shared_output);
                                if !RUNNING.read().load(Ordering::SeqCst) {
                                    break;
                                }
                            }
                        }
                        Ok(Err(e)) => error!("Unix accept error: {}", e),
                        Err(_) => continue,
                    }
                }
            }
            other => {
                return Err(std::io::Error::other(format!(
                    "Unsupported socket mode: {}",
                    other
                )));
            }
        }

        Ok(())
    }

    fn execution_contract(&self) -> crate::plugins::SourceExecutionContract {
        crate::plugins::SourceExecutionContract::stream(
            crate::plugins::SourceOnceContract::HostIdleBounded,
        )
    }
}
