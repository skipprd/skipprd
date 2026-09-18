use skippr_runtime_sdk::SkipprConfig;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
    Router,
};
use serde_derive::Deserialize;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tracing::info;

use crate::helpers::plugin_config::PluginConfigEntry;
use crate::RUNNING;
use skippr_runtime_sdk::plugins::DataSource;
use skippr_runtime_sdk::progress::OffsetKey;
use skippr_runtime_sdk::source_compat::{submit_payload_batches, IngestBatch, SourceSyncContext};

#[derive(Debug, Deserialize, SkipprConfig, Clone)]
pub struct DataSourceHttpServerPluginConfig {
    pub listen_address: Option<String>,
    pub path: Option<String>,
    #[skippr(secret)]
    pub auth_token: Option<String>,
    pub format: Option<String>,
    pub batch_size_bytes: Option<i64>,
    pub batch_size_seconds: Option<i64>,
}

impl TryFrom<PluginConfigEntry> for DataSourceHttpServerPluginConfig {
    type Error = String;

    fn try_from(plugin_config: PluginConfigEntry) -> Result<Self, Self::Error> {
        plugin_config.decode_for_plugin("HttpServer")
    }
}

#[derive(Clone)]
struct AppState {
    tx: mpsc::UnboundedSender<String>,
    auth_token: Option<String>,
}

async fn ingest_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    if let Some(ref expected) = state.auth_token {
        let provided = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let expected_header = format!("Bearer {}", expected);
        if provided != expected_header {
            return StatusCode::UNAUTHORIZED;
        }
    }
    let text = String::from_utf8_lossy(&body).into_owned();
    if text.is_empty() {
        return StatusCode::BAD_REQUEST;
    }
    let _ = state.tx.send(text);
    StatusCode::OK
}

pub struct DataSourceHttpServerPlugin {
    config: DataSourceHttpServerPluginConfig,
}

impl DataSourceHttpServerPlugin {

    pub fn with_runtime_config(config: DataSourceHttpServerPluginConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl DataSource for DataSourceHttpServerPlugin {
    async fn sync(&mut self, ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error> {
        let addr = self
            .config
            .listen_address
            .clone()
            .unwrap_or_else(|| "0.0.0.0:8080".to_string());
        let path = self.config.path.clone().unwrap_or_else(|| "/".to_string());

        let (tx, mut rx) = mpsc::unbounded_channel::<String>();

        let state = AppState {
            tx,
            auth_token: self.config.auth_token.clone(),
        };

        let app = Router::new()
            .route(&path, post(ingest_handler))
            .with_state(state);

        let listener = TcpListener::bind(&addr)
            .await
            .map_err(|e| std::io::Error::other(format!("Failed to bind {}: {}", addr, e)))?;
        info!("HttpServer listening on {}{}", addr, path);

        let server_handle = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        let mut counter: u64 = 0;
        while RUNNING.read().load(Ordering::SeqCst) {
            match tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv()).await {
                Ok(Some(data)) => {
                    counter += 1;
                    let bytes = data.len();
                    let offset_key = OffsetKey {
                        namespace: "http_server".to_string(),
                        partition: counter.to_string(),
                    };
                    submit_payload_batches(
                        ctx.as_ref(),
                        vec![IngestBatch {
                            offset_key,
                            data,
                            bytes,
                            offset_pos: None,
                            source_uri: format!("http_server://{}{}", addr, path),
                            namespace: Some("http_server".to_string()),
                            cdc_rows: None,
                        }],
                    )?;
                }
                Ok(None) => break,
                Err(_) => continue,
            }
        }

        server_handle.abort();
        Ok(())
    }

    fn execution_contract(&self) -> skippr_runtime_sdk::plugins::SourceExecutionContract {
        skippr_runtime_sdk::plugins::SourceExecutionContract::stream(
            skippr_runtime_sdk::plugins::SourceOnceContract::HostIdleBounded,
        )
    }
}
