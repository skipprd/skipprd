use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use serde_json::{json, Value};
use skippr_plugin_shared_api_source::OpenAiChatClient;
use skippr_runtime_sdk::helpers::offsets::OffsetKey;
use skippr_runtime_sdk::plugins::{
    DataSource, SourceExecutionContract, SourceOnceContract, SourceSyncContext,
};
use skippr_runtime_sdk::protocol::SKIPPR_RUNTIME_EXECUTION_MODE_ENV;
use skippr_runtime_sdk::source_compat::{submit_payload_batches, IngestBatch};
use tracing::info;

use crate::checkpoint::{
    blocks_needing_analysis, content_unchanged, load_page_checkpoint, store_page_checkpoint,
    PageContentCheckpoint, PageScores,
};
use crate::config::DataSourceSeoCrawlPluginConfig;
use crate::crawler::{crawl_site, seed_origin};
use crate::fetch::HttpFetcher;
use crate::html::{rollup_page_scores, ContentBlock};
use crate::openai_blocks::analyze_blocks_batch;
use crate::origin::SiteOrigin;
use crate::robots::parse_robots_txt;
use crate::scorecard::{page_check_rows, site_check_rows};
use crate::streams::{
    all_namespace_contracts, NAMESPACE_CHECK_DAILY, NAMESPACE_CONTENT_BLOCK, NAMESPACE_LINK_EDGE,
    NAMESPACE_PAGE_DAILY, NAMESPACE_ROBOTS_TXT, NAMESPACE_SITE_RUN_DAILY, NAMESPACE_SITEMAP_URL,
};

const DISCOVER_MAX_PAGES: u32 = 10;

fn runtime_is_discover_mode() -> bool {
    std::env::var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV)
        .map(|m| m.eq_ignore_ascii_case("discover"))
        .unwrap_or(false)
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
        if config.render_js {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "render_js is not supported in seo_crawl (use site_quality for JS rendering)",
            ));
        }
        let origin = seed_origin(&config.site)?;
        let openai = if config.openai_active() {
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
        let user_agent = config.user_agent.clone();
        Ok(Self {
            config,
            origin,
            fetcher: HttpFetcher::new(&user_agent),
            openai,
        })
    }

    pub fn with_openai(mut self, client: OpenAiChatClient) -> Self {
        self.openai = Some(client);
        self
    }

    fn crawl_date() -> String {
        Utc::now().format("%Y-%m-%d").to_string()
    }

    fn submit_rows(
        &self,
        ctx: &dyn SourceSyncContext,
        namespace: &str,
        crawl_date: &str,
        rows: Vec<Value>,
    ) -> Result<(), std::io::Error> {
        if rows.is_empty() {
            return Ok(());
        }
        let payload = rows
            .iter()
            .map(|row| serde_json::to_string(row))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| std::io::Error::other(e.to_string()))?
            .join("\n");
        let bytes = payload.len();
        submit_payload_batches(
            ctx,
            vec![IngestBatch {
                offset_key: OffsetKey::new(namespace, crawl_date.to_string()),
                data: payload,
                bytes,
                source_uri: format!("seo-crawl://{}", self.origin.origin),
                namespace: Some(namespace.to_string()),
                cdc_rows: None,
            }],
        )?;
        Ok(())
    }

    async fn emit_discovery_rows(
        &self,
        crawl_date: &str,
        run_id: &str,
    ) -> Result<(Option<Value>, Vec<Value>), std::io::Error> {
        let robots_url = format!("{}/robots.txt", self.origin.origin.trim_end_matches('/'));
        let robots_resp = self.fetcher.get(&robots_url, &self.origin).await.ok();
        let robots_row = robots_resp.as_ref().map(|r| {
            let parsed = parse_robots_txt(&r.body);
            json!({
                "site": self.origin.origin,
                "crawl_date": crawl_date,
                "run_id": run_id,
                "http_status": r.status,
                "raw_body": r.body,
                "sitemap_urls": parsed.sitemap_urls,
            })
        });

        let mut sitemap_rows = Vec::new();
        let probe_paths = &self.config.sitemap_probe_paths;
        let mut sitemap_files: Vec<String> = robots_resp
            .as_ref()
            .map(|r| parse_robots_txt(&r.body).sitemap_urls)
            .unwrap_or_default();
        for path in probe_paths {
            sitemap_files.push(format!(
                "{}{}",
                self.origin.origin.trim_end_matches('/'),
                path
            ));
        }
        sitemap_files.sort();
        sitemap_files.dedup();

        for sm_url in sitemap_files {
            if let Ok(resp) = self.fetcher.get(&sm_url, &self.origin).await {
                if let Ok(entries) = crate::sitemap::parse_sitemap_xml(&resp.body) {
                    for entry in entries {
                        sitemap_rows.push(json!({
                            "site": self.origin.origin,
                            "crawl_date": crawl_date,
                            "run_id": run_id,
                            "sitemap_file": sm_url,
                            "page_url": entry.loc,
                            "lastmod": entry.lastmod,
                        }));
                    }
                }
            }
        }
        Ok((robots_row, sitemap_rows))
    }
}

#[async_trait]
impl DataSource for DataSourceSeoCrawlPlugin {
    fn execution_contract(&self) -> SourceExecutionContract {
        SourceExecutionContract::stream(SourceOnceContract::Finite)
    }

    fn source_namespace_contracts(
        &self,
    ) -> Vec<skippr_runtime_sdk::plugins::source_contract::SourceNamespaceContract> {
        all_namespace_contracts()
    }

    async fn sync(&mut self, ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error> {
        self.config.validate()?;
        let discover = runtime_is_discover_mode();
        let max_urls = if discover {
            DISCOVER_MAX_PAGES
        } else {
            self.config.max_urls
        };
        let max_depth = if discover { 2 } else { self.config.max_depth };
        if discover {
            info!(
                max_urls,
                "SeoCrawl discover: minimal crawl (~10 pages), no OpenAI, no checkpoint advance"
            );
        }

        let crawl_date = Self::crawl_date();
        let run_id = Utc::now().to_rfc3339();
        let site = self.origin.origin.clone();

        let (robots_row, sitemap_rows) = self.emit_discovery_rows(&crawl_date, &run_id).await?;

        let pages = crawl_site(
            &self.origin,
            &self.fetcher,
            &self.config.user_agent,
            max_urls,
            max_depth,
            self.config.respect_robots,
            &self.config.seed_urls,
        )
        .await?;

        let mut page_rows = Vec::new();
        let mut block_rows = Vec::new();
        let mut link_rows = Vec::new();
        let mut issue_rows = Vec::new();
        let mut status_histogram: HashMap<u16, u64> = HashMap::new();
        let mut technical_scores = Vec::new();

        for page in pages {
            *status_histogram.entry(page.status).or_insert(0) += 1;
            let url = page.parsed.canonical_url.clone();
            let checkpoint = if discover {
                None
            } else {
                load_page_checkpoint(ctx.as_ref(), &url)
            };
            let unchanged = content_unchanged(true, checkpoint.as_ref(), &page.parsed.content_hash);

            let mut block_scores_json: Vec<Value> = if unchanged {
                checkpoint
                    .as_ref()
                    .map(|cp| cp.block_scores.values().cloned().collect())
                    .unwrap_or_default()
            } else {
                let to_analyze: Vec<ContentBlock> = if discover {
                    page.parsed.blocks.clone()
                } else {
                    blocks_needing_analysis(&page.parsed.blocks, checkpoint.as_ref())
                        .into_iter()
                        .cloned()
                        .collect()
                };
                let cap = self.config.openai_max_blocks_per_page.max(1) as usize;
                let to_analyze: Vec<ContentBlock> =
                    to_analyze.into_iter().take(cap).collect();
                if !discover && self.config.openai_active() && !to_analyze.is_empty() {
                    if let Some(client) = &self.openai {
                        analyze_blocks_batch(
                            client,
                            &self.config.openai_model,
                            &site,
                            &url,
                            &to_analyze,
                        )
                        .await?
                    } else {
                        to_analyze
                            .iter()
                            .map(crate::html::mock_block_analysis)
                            .collect()
                    }
                } else if !to_analyze.is_empty() && !discover {
                    to_analyze
                        .iter()
                        .map(crate::html::mock_block_analysis)
                        .collect()
                } else {
                    Vec::new()
                }
            };

            if !unchanged && block_scores_json.is_empty() && !page.parsed.blocks.is_empty() {
                block_scores_json = page
                    .parsed
                    .blocks
                    .iter()
                    .map(crate::html::mock_block_analysis)
                    .collect();
            }

            let rollup = rollup_page_scores(&block_scores_json);
            let mut page_scores = PageScores {
                technical_score: page.parsed.technical_score,
                content_quality_score: rollup.content_quality_score,
                eeat_proxy_score: rollup.eeat_proxy_score,
                ai_readiness_score: rollup.ai_readiness_score,
                risk_score: (1.0 - rollup.content_quality_score).clamp(0.0, 1.0),
            };
            if unchanged {
                if let Some(cp) = checkpoint.as_ref() {
                    page_scores = cp.page_scores.clone();
                }
            }
            technical_scores.push(page_scores.technical_score);

            page_rows.push(json!({
                "site": site,
                "crawl_date": crawl_date,
                "run_id": run_id,
                "canonical_url": url,
                "final_url": page.parsed.canonical_url,
                "status": page.status,
                "head": page.parsed.head,
                "http_headers": page.parsed.http_headers,
                "content_hash": page.parsed.content_hash,
                "content_unchanged": unchanged,
                "technical_score": page_scores.technical_score,
                "content_quality_score": page_scores.content_quality_score,
                "eeat_proxy_score": page_scores.eeat_proxy_score,
                "ai_readiness_score": page_scores.ai_readiness_score,
                "risk_score": page_scores.risk_score,
                "structured_data_count": page.parsed.structured_data_count,
            }));

            for (block, score) in page.parsed.blocks.iter().zip(block_scores_json.iter()) {
                block_rows.push(json!({
                    "site": site,
                    "crawl_date": crawl_date,
                    "run_id": run_id,
                    "page_url": url,
                    "block_id": block.block_id,
                    "block_type": block.block_type,
                    "heading_path": block.heading_path,
                    "char_count": block.char_count,
                    "word_count": block.word_count,
                    "text_hash": block.text_hash,
                    "content_unchanged": unchanged,
                    "extractability_score": score.get("extractability_score"),
                    "answer_clarity_score": score.get("answer_clarity_score"),
                    "citation_worthiness_score": score.get("citation_worthiness_score"),
                    "helpfulness_score": score.get("helpfulness_score"),
                    "trust_score": score.get("trust_score"),
                    "is_self_contained": score.get("is_self_contained"),
                    "confidence": score.get("confidence"),
                }));
            }

            for link in &page.parsed.internal_links {
                link_rows.push(json!({
                    "site": site,
                    "crawl_date": crawl_date,
                    "run_id": run_id,
                    "source_url": page.url,
                    "target_url": link.target_url,
                    "final_target_url": link.target_url,
                    "link_kind": link.link_kind,
                    "anchor_text": link.anchor_text,
                    "rel": link.rel,
                    "is_nofollow": link.is_nofollow,
                    "is_ugc": false,
                    "is_sponsored": false,
                    "position": "main_content",
                }));
            }

            issue_rows.extend(page_check_rows(
                &site,
                &crawl_date,
                &url,
                &page.parsed,
                page.status,
            ));

            if !discover {
                let mut block_hashes = HashMap::new();
                let mut block_scores = HashMap::new();
                for block in &page.parsed.blocks {
                    block_hashes.insert(block.block_id.clone(), block.text_hash.clone());
                }
                for (block, score) in page.parsed.blocks.iter().zip(block_scores_json.iter()) {
                    block_scores.insert(block.block_id.clone(), score.clone());
                }
                store_page_checkpoint(
                    ctx.as_ref(),
                    &url,
                    &PageContentCheckpoint {
                        content_hash: page.parsed.content_hash.clone(),
                        last_crawl_date: crawl_date.clone(),
                        page_scores,
                        block_hashes,
                        block_scores,
                        openai_model: self.config.openai_model.clone(),
                    },
                )?;
            }
        }

        let mean_technical = if technical_scores.is_empty() {
            0.0
        } else {
            technical_scores.iter().sum::<f64>() / technical_scores.len() as f64
        };

        let site_row = json!({
            "site": site,
            "crawl_date": crawl_date,
            "run_id": run_id,
            "urls_discovered": page_rows.len(),
            "urls_fetched": page_rows.len(),
            "robots_txt_found": robots_row.is_some(),
            "sitemap_url_rows": sitemap_rows.len(),
            "status_code_histogram": status_histogram,
            "mean_technical_score": mean_technical,
        });

        let summary = (
            page_rows.len(),
            link_rows.len(),
            issue_rows.len(),
            block_rows.len(),
            sitemap_rows.len(),
        );

        issue_rows.extend(site_check_rows(
            &site,
            &crawl_date,
            robots_row.is_some(),
            sitemap_rows.len(),
        ));

        if let Some(row) = robots_row {
            self.submit_rows(ctx.as_ref(), NAMESPACE_ROBOTS_TXT, &crawl_date, vec![row])?;
        }
        self.submit_rows(
            ctx.as_ref(),
            NAMESPACE_SITEMAP_URL,
            &crawl_date,
            sitemap_rows,
        )?;
        self.submit_rows(ctx.as_ref(), NAMESPACE_PAGE_DAILY, &crawl_date, page_rows)?;
        self.submit_rows(ctx.as_ref(), NAMESPACE_LINK_EDGE, &crawl_date, link_rows)?;
        self.submit_rows(ctx.as_ref(), NAMESPACE_CHECK_DAILY, &crawl_date, issue_rows)?;
        self.submit_rows(
            ctx.as_ref(),
            NAMESPACE_CONTENT_BLOCK,
            &crawl_date,
            block_rows,
        )?;
        self.submit_rows(
            ctx.as_ref(),
            NAMESPACE_SITE_RUN_DAILY,
            &crawl_date,
            vec![site_row],
        )?;
        info!(
            site = %site,
            crawl_date = %crawl_date,
            pages = summary.0,
            links = summary.1,
            issues = summary.2,
            blocks = summary.3,
            sitemap_urls = summary.4,
            "SeoCrawl sync complete"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use skippr_runtime_sdk::plugins::cdc::CheckpointEnvelope;
    use skippr_runtime_sdk::plugins::{OffsetValidationEntry, SourcePayloadTask, SourceSyncContext};
    use skippr_runtime_sdk::protocol::RuntimeOffsetMaterializationHint;
    use skippr_runtime_sdk::source_compat::ThroughputMetrics;

    use crate::checkpoint::checkpoint_key;
    use crate::html::{content_hash, ContentBlock};
    use crate::openai_blocks::CountingOpenAiClient;

    static ENV_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn test_config() -> DataSourceSeoCrawlPluginConfig {
        DataSourceSeoCrawlPluginConfig {
            site: "https://example.com".into(),
            max_urls: 5,
            max_depth: 2,
            render_js: false,
            crawl_rate_per_second: 100.0,
            respect_robots: true,
            sitemap_probe_paths: default_sitemap_probe_paths(),
            openai_model: "gpt-4.1-mini".into(),
            openai_enabled: true,
            openai_analyze_blocks: true,
            openai_max_blocks_per_page: 24,
            skip_unchanged_content: true,
            user_agent: "SkipprSeoCrawl/test".into(),
            seed_urls: Vec::new(),
        }
    }

    fn default_sitemap_probe_paths() -> Vec<String> {
        vec!["/sitemap.xml".into()]
    }

    #[derive(Default)]
    struct MemorySyncContext {
        checkpoints: Mutex<HashMap<String, CheckpointEnvelope>>,
        writes: Mutex<Vec<String>>,
    }

    impl SourceSyncContext for MemorySyncContext {
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
            envelope: &CheckpointEnvelope,
        ) -> Result<(), String> {
            self.writes.lock().unwrap().push(key.to_string());
            self.checkpoints
                .lock()
                .unwrap()
                .insert(key.to_string(), envelope.clone());
            Ok(())
        }

        fn load_checkpoint_envelope(&self, key: &str) -> Option<CheckpointEnvelope> {
            self.checkpoints.lock().unwrap().get(key).cloned()
        }
    }

    #[test]
    fn checkpoint_roundtrip_in_offset_store() {
        let ctx = MemorySyncContext::default();
        let url = "https://example.com/page";
        store_page_checkpoint(
            &ctx,
            url,
            &PageContentCheckpoint {
                content_hash: "sha256:abc".into(),
                last_crawl_date: "2026-05-29".into(),
                page_scores: PageScores::default(),
                block_hashes: HashMap::new(),
                block_scores: HashMap::new(),
                openai_model: "gpt-4.1-mini".into(),
            },
        )
        .unwrap();
        let loaded = load_page_checkpoint(&ctx, url).unwrap();
        assert_eq!(loaded.content_hash, "sha256:abc");
        assert_eq!(ctx.writes.lock().unwrap()[0], checkpoint_key(url));
    }

    #[tokio::test]
    async fn content_hash_unchanged_skips_openai_calls() {
        let _lock = env_lock();
        let fixture_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        std::env::set_var("SKIPPR_SEO_CRAWL_FIXTURE_DIR", fixture_dir);
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);

        let html = std::fs::read_to_string(format!(
            "{fixture_dir}/pages/0f115db062b7c0dd030b16878c99dea5c354b49dc37b38eb8846179c7783e9d7.html"
        ))
        .expect("fixture html");
        let origin = crate::origin::normalize_site("https://example.com").unwrap();
        let response = crate::fetch::FetchResponse {
            final_url: "https://example.com/".into(),
            status: 200,
            headers: HashMap::new(),
            body: html,
            redirect_chain: vec![200],
            ttfb_ms: 1,
        };
        let parsed = crate::html::parse_fetched_page("https://example.com/", &response, &origin);
        let ctx = Arc::new(MemorySyncContext::default());
        store_page_checkpoint(
            ctx.as_ref(),
            "https://example.com/",
            &PageContentCheckpoint {
                content_hash: parsed.content_hash.clone(),
                last_crawl_date: "2026-05-28".into(),
                page_scores: PageScores {
                    technical_score: 0.9,
                    content_quality_score: 0.7,
                    eeat_proxy_score: 0.71,
                    ai_readiness_score: 0.82,
                    risk_score: 0.18,
                },
                block_hashes: parsed
                    .blocks
                    .iter()
                    .map(|b| (b.block_id.clone(), b.text_hash.clone()))
                    .collect(),
                block_scores: HashMap::new(),
                openai_model: "gpt-4.1-mini".into(),
            },
        )
        .unwrap();

        let counting = CountingOpenAiClient::wrap(OpenAiChatClient::new(
            "fixture",
            "https://api.openai.com/v1",
        ));
        let calls = counting.calls.clone();
        let mut plugin = DataSourceSeoCrawlPlugin::new(test_config()).unwrap();
        plugin.openai = Some(counting.inner);
        plugin.config.max_urls = 1;
        plugin.sync(ctx).await.unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);

        std::env::remove_var("SKIPPR_SEO_CRAWL_FIXTURE_DIR");
    }

    #[tokio::test]
    async fn block_hash_change_analyzes_only_delta_blocks() {
        let unchanged = ContentBlock {
            block_id: "b1".into(),
            block_type: "heading_section".into(),
            heading_path: vec![],
            text: "same text".into(),
            text_hash: content_hash("same text"),
            char_count: 9,
            word_count: 2,
            ordinal: 0,
            has_list: false,
            has_table: false,
            has_citation: false,
            outbound_link_count: 0,
        };
        let changed = ContentBlock {
            block_id: "b2".into(),
            block_type: "direct_answer".into(),
            heading_path: vec![],
            text: "new text".into(),
            text_hash: content_hash("new text"),
            char_count: 8,
            word_count: 2,
            ordinal: 1,
            has_list: false,
            has_table: false,
            has_citation: false,
            outbound_link_count: 0,
        };
        let cp = PageContentCheckpoint {
            content_hash: "sha256:page".into(),
            last_crawl_date: "2026-05-28".into(),
            page_scores: PageScores::default(),
            block_hashes: HashMap::from([("b1".into(), unchanged.text_hash.clone())]),
            block_scores: HashMap::new(),
            openai_model: "gpt-4.1-mini".into(),
        };
        let blocks = [unchanged, changed];
        let delta = blocks_needing_analysis(&blocks, Some(&cp));
        assert_eq!(delta.len(), 1);
        assert_eq!(delta[0].block_id, "b2");
    }

    #[test]
    fn discover_sync_does_not_store_checkpoints() {
        let _lock = env_lock();
        let fixture_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        std::env::set_var("SKIPPR_SEO_CRAWL_FIXTURE_DIR", fixture_dir);
        std::env::set_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV, "discover");

        let mut plugin = DataSourceSeoCrawlPlugin::new(test_config()).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ctx = Arc::new(MemorySyncContext::default());
        rt.block_on(plugin.sync(ctx.clone())).expect("discover sync");
        assert!(ctx.writes.lock().unwrap().is_empty());

        std::env::remove_var("SKIPPR_SEO_CRAWL_FIXTURE_DIR");
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);
    }
}
