use skippr_runtime_sdk::SkipprConfig;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;
use serde_derive::Deserialize;
use tokio::io::AsyncBufReadExt;
use tokio::net::{TcpListener, UdpSocket};
use tokio::time::Duration;
use tracing::{error, info};

use crate::helpers::plugin_config::PluginConfigEntry;
use crate::RUNNING;
use skippr_runtime_sdk::plugins::DataSource;
use skippr_runtime_sdk::progress::OffsetKey;
use skippr_runtime_sdk::source_compat::{submit_payload_batches, IngestBatch, SourceSyncContext};

#[derive(Debug, Deserialize, SkipprConfig, Clone)]
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
    config: DataSourceSocketPluginConfig,
}

struct LineBatcher {
    buffer: Vec<u8>,
    limit: usize,
}

impl LineBatcher {
    fn new(limit: usize) -> Self {
        Self {
            buffer: Vec::new(),
            limit: limit.max(1),
        }
    }

    fn push_line(&mut self, line: &str) -> Option<Vec<u8>> {
        if line.is_empty() {
            return None;
        }
        self.buffer.extend(line.as_bytes());
        self.buffer.push(b'\n');
        if self.buffer.len() >= self.limit {
            Some(std::mem::take(&mut self.buffer))
        } else {
            None
        }
    }

    fn take_rest(&mut self) -> Option<Vec<u8>> {
        if self.buffer.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.buffer))
        }
    }
}

impl DataSourceSocketPlugin {
    pub fn with_runtime_config(config: DataSourceSocketPluginConfig) -> Self {
        Self { config }
    }

    fn batch_limit(&self) -> usize {
        self.config.batch_size_bytes.unwrap_or(1).max(1) as usize
    }

    fn dispatch_batch(
        &self,
        buffer: Vec<u8>,
        counter: &mut u64,
        ctx: &dyn SourceSyncContext,
    ) -> Result<(), std::io::Error> {
        if buffer.is_empty() {
            return Ok(());
        }
        *counter += 1;
        let data = String::from_utf8_lossy(&buffer).into_owned();
        let bytes = data.len();
        let namespace = format!("socket.{}.{}", self.config.mode, self.config.address);
        let offset_key = OffsetKey {
            namespace: namespace.clone(),
            partition: counter.to_string(),
        };
        submit_payload_batches(
            ctx,
            vec![IngestBatch {
                offset_key,
                data,
                bytes,
                offset_pos: None,
                source_uri: format!("socket://{}:{}", self.config.mode, self.config.address),
                namespace: Some(namespace),
                cdc_rows: None,
            }],
        )
        .map(|_| ())
    }

    fn ingest_line(
        &self,
        batcher: &mut LineBatcher,
        line: String,
        counter: &mut u64,
        ctx: &dyn SourceSyncContext,
    ) -> Result<(), std::io::Error> {
        if let Some(buffer) = batcher.push_line(&line) {
            self.dispatch_batch(buffer, counter, ctx)?;
        }
        Ok(())
    }

    fn flush_batcher(
        &self,
        batcher: &mut LineBatcher,
        counter: &mut u64,
        ctx: &dyn SourceSyncContext,
    ) -> Result<(), std::io::Error> {
        if let Some(buffer) = batcher.take_rest() {
            self.dispatch_batch(buffer, counter, ctx)?;
        }
        Ok(())
    }
}

#[async_trait]
impl DataSource for DataSourceSocketPlugin {
    async fn sync(&mut self, ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error> {
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
                            let mut batcher = LineBatcher::new(self.batch_limit());
                            while let Ok(Some(line)) = lines.next_line().await {
                                self.ingest_line(&mut batcher, line, &mut counter, ctx.as_ref())?;
                                if !RUNNING.read().load(Ordering::SeqCst) {
                                    break;
                                }
                            }
                            self.flush_batcher(&mut batcher, &mut counter, ctx.as_ref())?;
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
                let mut batcher = LineBatcher::new(self.batch_limit());
                while RUNNING.read().load(Ordering::SeqCst) {
                    match tokio::time::timeout(Duration::from_secs(1), socket.recv_from(&mut buf))
                        .await
                    {
                        Ok(Ok((len, _addr))) => {
                            let data = String::from_utf8_lossy(&buf[..len]).into_owned();
                            for line in data.lines() {
                                self.ingest_line(
                                    &mut batcher,
                                    line.to_string(),
                                    &mut counter,
                                    ctx.as_ref(),
                                )?;
                            }
                            self.flush_batcher(&mut batcher, &mut counter, ctx.as_ref())?;
                        }
                        Ok(Err(e)) => error!("UDP recv error: {}", e),
                        Err(_) => continue,
                    }
                }
                self.flush_batcher(&mut batcher, &mut counter, ctx.as_ref())?;
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
                            let mut batcher = LineBatcher::new(self.batch_limit());
                            while let Ok(Some(line)) = lines.next_line().await {
                                self.ingest_line(&mut batcher, line, &mut counter, ctx.as_ref())?;
                                if !RUNNING.read().load(Ordering::SeqCst) {
                                    break;
                                }
                            }
                            self.flush_batcher(&mut batcher, &mut counter, ctx.as_ref())?;
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

    fn execution_contract(&self) -> skippr_runtime_sdk::plugins::SourceExecutionContract {
        skippr_runtime_sdk::plugins::SourceExecutionContract::stream(
            skippr_runtime_sdk::plugins::SourceOnceContract::HostIdleBounded,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::LineBatcher;

    #[test]
    fn line_batcher_holds_rows_until_byte_limit() {
        let mut batcher = LineBatcher::new(10);
        assert!(batcher.push_line(r#"{"id":1}"#).is_none());
        let flushed = batcher
            .push_line(r#"{"id":2}"#)
            .expect("second line crosses 10 bytes");
        let text = String::from_utf8(flushed).unwrap();
        assert!(text.contains(r#"{"id":1}"#));
        assert!(text.contains(r#"{"id":2}"#));
        assert_eq!(text.matches('\n').count(), 2);
        assert!(batcher.take_rest().is_none());
    }

    #[test]
    fn line_batcher_flushes_remainder_on_connection_close() {
        let mut batcher = LineBatcher::new(1_048_576);
        assert!(batcher.push_line(r#"{"id":1}"#).is_none());
        assert!(batcher.push_line(r#"{"id":2}"#).is_none());
        let rest = batcher.take_rest().expect("remainder");
        let text = String::from_utf8(rest).unwrap();
        assert_eq!(text.matches('\n').count(), 2);
        assert!(batcher.take_rest().is_none());
    }

    #[test]
    fn line_batcher_skips_empty_lines() {
        let mut batcher = LineBatcher::new(8);
        assert!(batcher.push_line("").is_none());
        assert!(batcher.take_rest().is_none());
    }
}
