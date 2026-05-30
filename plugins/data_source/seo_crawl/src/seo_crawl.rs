use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use serde::Deserialize;
use serde_derive::Serialize;
use serde_json::{json, Value};
use skippr_plugin_shared_api_source::OpenAiChatClient;
use skippr_runtime_sdk::helpers::offsets::OffsetKey;
use skippr_runtime_sdk::plugins::{
    DataSource, SourceExecutionContract, SourceOnceContract, SourceSyncContext,
};
use skippr_runtime_sdk::protocol::SKIPPR_RUNTIME_EXECUTION_MODE_ENV;
use skippr_runtime_sdk::source_compat::{submit_payload_batches, IngestBatch};
use tracing::info;

use crate::blocks::{block_row, extract_content_blocks};
use crate::checkpoint::{
    content_unchanged, load_page_checkpoint, store_page_checkpoint, PageContentCheckpoint,
};
use crate::crawler::{crawl_site, seed_origin};
use crate::fetch::HttpFetcher;
use crate::html::technical_score;
use crate::openai_blocks::analyze_block;
use crate::origin::SiteOrigin;
use crate::streams::{all_namespace_contracts, NAMESPACE_CONTENT_BLOCK, NAMESPACE_PAGE_DAILY};

const DISCOVER_MAX_URLS: u32 = 3;

fn runtime_is_discover_mode() -> bool {
    std::env::var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV)
        .map(|m| m.eq_ignore_ascii_case("discover"))
        .unwrap_or(false)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataSourceSeoCrawlPluginConfig {
    pub site: String,
    #[serde(default = "default_max_urls")]
    pub max_urls: u32,
    #[serde(default = "default_max_depth")]
    pub max_depth: u32,
    #[serde(default = "default_user_agent")]
    pub user_agent: String,
    #[serde(default = "default_respect_robots")]
    pub respect_robots: bool,
    #[serde(default = "default_openai_enabled")]
    pub openai_enabled: bool,
    #[serde(default = "default_openai_model")]
    pub openai_model: String,
    #[serde(default = "default_openai_analyze_blocks")]
    pub openai_analyze_blocks: bool,
    #[serde(default = "default_openai_max_blocks_per_page")]
    pub openai_max_blocks_per_page: u32,
    #[serde(default = "default_skip_unchanged_content")]
    pub skip_unchanged_content: bool,
}

fn default_max_urls() -> u32 {
    500
}
fn default_max_depth() -> u32 {
    8
}
fn default_user_agent() -> String {
    "SkipprSeoCrawl/1.0".into()
}
fn default_respect_robots() -> bool {
    true
}
fn default_openai_enabled() -> bool {
    true
}
fn default_openai_model() -> String {
    "gpt-4.1-mini".into()
}
fn default_openai_analyze_blocks() -> bool {
    true
}
fn default_openai_max_blocks_per_page() -> u32 {
    24
}
fn default_skip_unchanged_content() -> bool {
    true
}

pub struct DataSourceSeoCrawlPlugin {
    config: DataSourceSeoCrawlPluginConfig,
    origin: SiteOrigin,
    fetcher: HttpFetcher,
    openai: Option<OpenAiChatClient>,
}

impl DataSourceSeoCrawlPlugin {
    pub fn new(config: DataSourceSeoCrawlPluginConfig) -> Result<Self, std::io::Error> {
        config.validate()?;
        let origin = seed_origin(&config.site)?;
        let openai = if config.openai_enabled && config.openai_analyze_blocks {
            if std::env::var("SKIPPR_SEO_CRAWL_FIXTURE_DIR")
                .map(|d| !d.trim().is_empty())
                .unwrap_or(false)
            {
                Some(OpenAiChatClient::new("fixture", "https://api.openai.com/v1"))
            } else {
                OpenAiChatClient::from_env().ok()
            }
        } else {
            None
        };
        Ok(Self {
            config,
            origin,
            fetcher: HttpFetcher::new("SkipprSeoCrawl/1.0"),
            openai,
        })
    }

    fn crawl_date() -> String {
        Utc::now().format("%Y-%m-%d").to_string()
    }

    fn submit_namespace_rows(
        &self,
        ctx: &dyn SourceSyncContext,
        namespace: &str,
        rows: Vec<Value>,
    ) -> Result<(), std::io::Error> {
        if rows.is_empty() {
            return Ok(());
        }
        let crawl_date = Self::crawl_date();
        let payload = rows
            .into_iter()
            .map(|row| serde_json::to_string(&row))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| std::io::Error::other(e.to_string()))?
            .join("\n");
        let bytes = payload.len();
        submit_payload_batches(
            ctx,
            vec![IngestBatch {
                offset_key: OffsetKey::new(namespace, crawl_date.clone()),
                data: payload,
                bytes,
                source_uri: format!("seo-crawl://{}", self.origin.origin),
                namespace: Some(namespace.to_string()),
                cdc_rows: None,
            }],
        )?;
        Ok(())
    }
}

impl DataSourceSeoCrawlPluginConfig {
    pub fn validate(&self) -> Result<(), std::io::Error> {
        if self.site.trim().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "site is required",
            ));
        }
        if self.max_urls == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "max_urls must be >= 1",
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl DataSource for DataSourceSeoCrawlPlugin {
    fn execution_contract(&self) -> SourceExecutionContract {
        SourceExecutionContract::stream(SourceOnceContract::Finite)
    }

    fn source_namespace_contracts(&self) -> Vec<skippr_runtime_sdk::plugins::source_contract::SourceNamespaceContract> {
        all_namespace_contracts()
    }

    async fn sync(&mut self, ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error> {
        self.config.validate()?;
        let discover = runtime_is_discover_mode();
        let max_urls = if discover {
            DISCOVER_MAX_URLS
        } else {
            self.config.max_urls
        };
        let max_depth = if discover { 1 } else { self.config.max_depth };
        if discover {
            info!(
                max_urls,
                max_depth,
                "SeoCrawl discover: bounded crawl sample; no page checkpoints"
            );
        }

        let user_agent = self.config.user_agent.clone();
        let pages = crawl_site(
            &self.origin,
            &self.fetcher,
            &user_agent,
            max_urls,
            max_depth,
            self.config.respect_robots,
        )
        .await?;

        let crawl_date = Self::crawl_date();
        let site = self.origin.origin.clone();
        let mut page_rows = Vec::new();
        let mut block_rows = Vec::new();

        for page in pages {
            let url = page.parsed.canonical_url.clone();
            let unchanged = if discover {
                false
            } else {
                content_unchanged(
                    self.config.skip_unchanged_content,
                    load_page_checkpoint(ctx.as_ref(), &url).as_ref(),
                    &page.parsed.content_hash,
                )
            };

            page_rows.push(json!({
                "site": site,
                "crawl_date": crawl_date,
                "canonical_url": url,
                "content_hash": page.parsed.content_hash,
                "content_unchanged": unchanged,
                "technical_score": technical_score(
                    &crate::fetch::FetchResponse {
                        final_url: page.url.clone(),
                        status: page.status,
                        headers: Default::default(),
                        body: String::new(),
                        redirect_chain: vec![page.status],
                        ttfb_ms: 0,
                    },
                    &page.parsed,
                ),
                "head": page.parsed.head,
                "issue_count": page.parsed.issues.len(),
            }));

            let blocks = extract_content_blocks(&page.parsed);
            let block_hashes: Vec<String> = blocks.iter().map(|b| b.block_hash.clone()).collect();
            let block_cap = self.config.openai_max_blocks_per_page.max(1) as usize;
            for block in blocks.into_iter().take(block_cap) {
                let openai = if unchanged {
                    None
                } else if let Some(client) = &self.openai {
                    analyze_block(
                        client,
                        &self.config.openai_model,
                        &site,
                        &url,
                        &block,
                    )
                    .await
                    .ok()
                } else {
                    None
                };
                block_rows.push(block_row(
                    &site,
                    &crawl_date,
                    &url,
                    &block,
                    openai.as_ref(),
                    unchanged,
                ));
            }

            if !discover {
                store_page_checkpoint(
                    ctx.as_ref(),
                    &url,
                    &PageContentCheckpoint {
                        content_hash: page.parsed.content_hash.clone(),
                        block_hashes,
                    },
                )?;
            }
        }

        self.submit_namespace_rows(ctx.as_ref(), NAMESPACE_PAGE_DAILY, page_rows)?;
        self.submit_namespace_rows(ctx.as_ref(), NAMESPACE_CONTENT_BLOCK, block_rows)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use skippr_runtime_sdk::plugins::cdc::CheckpointEnvelope;
    use skippr_runtime_sdk::plugins::{OffsetValidationEntry, SourcePayloadTask, SourceSyncContext};
    use skippr_runtime_sdk::protocol::RuntimeOffsetMaterializationHint;
    use skippr_runtime_sdk::source_compat::ThroughputMetrics;
    use std::sync::Mutex;

    static ENV_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn env_test_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn test_config() -> DataSourceSeoCrawlPluginConfig {
        DataSourceSeoCrawlPluginConfig {
            site: "https://example.com".into(),
            max_urls: 5,
            max_depth: 1,
            user_agent: "SkipprSeoCrawl/1.0".into(),
            respect_robots: true,
            openai_enabled: true,
            openai_model: "gpt-4.1-mini".into(),
            openai_analyze_blocks: true,
            openai_max_blocks_per_page: 4,
            skip_unchanged_content: true,
        }
    }

    #[test]
    fn config_validation_rejects_empty_site() {
        let mut cfg = test_config();
        cfg.site = "  ".into();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn discover_sync_does_not_store_checkpoints() {
        let _lock = env_test_lock();
        let fixture_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        std::env::set_var("SKIPPR_SEO_CRAWL_FIXTURE_DIR", fixture_dir);
        std::env::set_var("SKIPPR_OPENAI_FIXTURE_DIR", fixture_dir);
        std::env::set_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV, "discover");

        let mut plugin = DataSourceSeoCrawlPlugin::new(test_config()).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        rt.block_on(plugin.sync(ctx.clone())).expect("discover sync");
        assert!(ctx.checkpoint_stores.lock().unwrap().is_empty());

        std::env::remove_var("SKIPPR_SEO_CRAWL_FIXTURE_DIR");
        std::env::remove_var("SKIPPR_OPENAI_FIXTURE_DIR");
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);
    }

    #[test]
    fn sync_mode_stores_page_checkpoints() {
        let _lock = env_test_lock();
        let fixture_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        std::env::set_var("SKIPPR_SEO_CRAWL_FIXTURE_DIR", fixture_dir);
        std::env::set_var("SKIPPR_OPENAI_FIXTURE_DIR", fixture_dir);
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);

        let mut plugin = DataSourceSeoCrawlPlugin::new(test_config()).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        rt.block_on(plugin.sync(ctx.clone())).expect("sync");
        assert!(!ctx.checkpoint_stores.lock().unwrap().is_empty());

        std::env::remove_var("SKIPPR_SEO_CRAWL_FIXTURE_DIR");
        std::env::remove_var("SKIPPR_OPENAI_FIXTURE_DIR");
    }

    #[derive(Default)]
    struct RecordingSyncContext {
        checkpoint_stores: Mutex<Vec<String>>,
    }

    impl SourceSyncContext for RecordingSyncContext {
        fn submit_payload_tasks(
            &self,
            _tasks: Vec<SourcePayloadTask>,
        ) -> Result<ThroughputMetrics, std::io::Error> {
            Ok(ThroughputMetrics {
                bytes_per_second: 0,
                active_cores: 0,
                queue_length: 0,
                optimal_chunk_size: 0,
            })
        }

        fn validate_offset_batch(
            &self,
            entries: &[OffsetValidationEntry],
        ) -> Result<Vec<bool>, std::io::Error> {
            Ok(vec![false; entries.len()])
        }

        fn relay_offset_hints(
            &self,
            _hints: Vec<RuntimeOffsetMaterializationHint>,
        ) -> Result<(), std::io::Error> {
            Ok(())
        }

        fn store_checkpoint(
            &self,
            key: &str,
            _envelope: &CheckpointEnvelope,
        ) -> Result<(), String> {
            self.checkpoint_stores.lock().unwrap().push(key.to_string());
            Ok(())
        }

        fn load_checkpoint_envelope(&self, _key: &str) -> Option<CheckpointEnvelope> {
            None
        }
    }
}
