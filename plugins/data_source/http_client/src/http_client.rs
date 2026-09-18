use skippr_runtime_sdk::SkipprConfig;
use std::collections::HashMap;
use std::io::{Cursor, Read as IoRead};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;
use flate2::read::GzDecoder;
use reqwest::Client;
use serde_derive::Deserialize;
use tokio::time::{sleep, Duration};
use tracing::{error, info};

use crate::helpers::plugin_config::PluginConfigEntry;
use crate::RUNNING;
use skippr_runtime_sdk::plugins::{DataSource, SourceExecutionContract, SourceOnceContract};
use skippr_runtime_sdk::progress::OffsetKey;
use skippr_runtime_sdk::source_compat::{
    submit_payload_batch_groups, IngestBatch, SourceSyncContext,
};

#[derive(Debug, Deserialize, SkipprConfig, Clone)]
pub struct DataSourceHttpAuthConfig {
    pub strategy: Option<String>,
    pub user: Option<String>,
    #[skippr(secret)]
    pub password: Option<String>,
    #[skippr(secret)]
    pub token: Option<String>,
}

#[derive(Debug, Deserialize, SkipprConfig, Clone)]
pub struct DataSourceHttpClientPluginConfig {
    pub url: String,
    pub method: Option<String>,
    pub headers: Option<HashMap<String, String>>,
    pub body: Option<String>,
    pub auth: Option<DataSourceHttpAuthConfig>,
    pub scrape_interval_seconds: Option<u64>,
    pub scrape_timeout_seconds: Option<u64>,
    pub format: Option<String>,
    pub batch_size_bytes: Option<i64>,
    pub batch_size_seconds: Option<i64>,
}

impl TryFrom<PluginConfigEntry> for DataSourceHttpClientPluginConfig {
    type Error = String;

    fn try_from(plugin_config: PluginConfigEntry) -> Result<Self, Self::Error> {
        plugin_config.decode_for_plugin("HttpClient")
    }
}

pub struct DataSourceHttpClientPlugin {
    client: Client,
    config: DataSourceHttpClientPluginConfig,
}

impl DataSourceHttpClientPlugin {

    pub fn with_runtime_config(config: DataSourceHttpClientPluginConfig) -> Self {
        Self {
            client: Client::new(),
            config,
        }
    }

    async fn fetch_once(&self, ctx: &dyn SourceSyncContext) -> Result<(), std::io::Error> {
        if self.config.url.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "http url is empty",
            ));
        }

        let method = self
            .config
            .method
            .as_deref()
            .unwrap_or("GET")
            .to_uppercase();
        let mut req = match method.as_str() {
            "POST" => self.client.post(&self.config.url),
            "PUT" => self.client.put(&self.config.url),
            _ => self.client.get(&self.config.url),
        };

        if let Some(ref headers) = self.config.headers {
            for (k, v) in headers {
                req = req.header(k.as_str(), v.as_str());
            }
        }

        if let Some(ref auth) = self.config.auth {
            match auth.strategy.as_deref() {
                Some("basic") => {
                    if let (Some(u), Some(p)) = (&auth.user, &auth.password) {
                        req = req.basic_auth(u, Some(p));
                    }
                }
                Some("bearer") => {
                    if let Some(t) = &auth.token {
                        req = req.bearer_auth(t);
                    }
                }
                _ => {}
            }
        }

        if let Some(ref body) = self.config.body {
            req = req.body(body.clone());
        }

        let timeout_secs = self.config.scrape_timeout_seconds.unwrap_or(5);
        req = req.timeout(Duration::from_secs(timeout_secs));

        let response = req
            .send()
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let response = response
            .error_for_status()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let body_bytes = response
            .bytes()
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let text = if self.config.url.contains(".gz") {
            let mut decoder = GzDecoder::new(Cursor::new(body_bytes.to_vec()));
            let mut decompressed = String::new();
            decoder
                .read_to_string(&mut decompressed)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            decompressed
        } else {
            String::from_utf8_lossy(&body_bytes).into_owned()
        };

        if text.is_empty() {
            return Ok(());
        }

        let offset_key = OffsetKey {
            namespace: self.config.url.clone(),
            partition: String::new(),
        };

        let chunk_bytes = self.config.batch_size_bytes.unwrap_or(1_024_000) as usize;
        let source_uri = self.config.url.clone();
        let mut groups: Vec<Vec<IngestBatch>> = Vec::new();
        let mut buf = String::new();

        for line in text.lines() {
            if !buf.is_empty() {
                buf.push('\n');
            }
            buf.push_str(line);
            if buf.len() >= chunk_bytes {
                let data = std::mem::take(&mut buf);
                let bytes = data.len();
                groups.push(vec![IngestBatch {
                    offset_key: offset_key.clone(),
                    data,
                    bytes,
                    offset_pos: None,
                    source_uri: source_uri.clone(),
                    namespace: Some("http".to_string()),
                    cdc_rows: None,
                }]);
            }
        }

        if !buf.is_empty() {
            let bytes = buf.len();
            groups.push(vec![IngestBatch {
                offset_key: offset_key.clone(),
                data: buf,
                bytes,
                offset_pos: None,
                source_uri: source_uri.clone(),
                namespace: Some("http".to_string()),
                cdc_rows: None,
            }]);
        }

        if !groups.is_empty() {
            submit_payload_batch_groups(ctx, groups)?;
        }

        Ok(())
    }
}

#[async_trait]
impl DataSource for DataSourceHttpClientPlugin {
    async fn sync(&mut self, ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error> {
        match self.config.scrape_interval_seconds {
            Some(interval) => {
                info!(
                    "HttpClient polling every {}s: {}",
                    interval, self.config.url
                );
                while RUNNING.read().load(Ordering::SeqCst) {
                    if let Err(e) = self.fetch_once(ctx.as_ref()).await {
                        error!("HttpClient fetch error: {}", e);
                    }
                    sleep(Duration::from_secs(interval)).await;
                }
                Ok(())
            }
            None => self.fetch_once(ctx.as_ref()).await,
        }
    }

    fn execution_contract(&self) -> SourceExecutionContract {
        if self.config.scrape_interval_seconds.is_some() {
            SourceExecutionContract::stream(SourceOnceContract::HostIdleBounded)
        } else {
            SourceExecutionContract::finite()
        }
    }
}
