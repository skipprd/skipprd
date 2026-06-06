use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use serde_json::{json, Value};
use skippr_runtime_sdk::helpers::offsets::OffsetKey;
use skippr_runtime_sdk::plugins::{
    DataSource, SourceExecutionContract, SourceOnceContract, SourceSyncContext,
};
use skippr_runtime_sdk::protocol::SKIPPR_RUNTIME_EXECUTION_MODE_ENV;
use skippr_runtime_sdk::source_compat::{submit_payload_batches, IngestBatch};
use tracing::info;

use crate::checkpoint::{
    content_unchanged, load_page_checkpoint, store_page_checkpoint, PageTechnicalCheckpoint,
};
use crate::config::DataSourceSeoCrawlPluginConfig;
use crate::crawler::{crawl_site, seed_origin};
use crate::fetch::HttpFetcher;
use crate::origin::SiteOrigin;
use crate::robots::parse_robots_txt;
use crate::scorecard::{page_check_rows, site_check_rows};
use crate::streams::{
    all_namespace_contracts, NAMESPACE_CHECK_DAILY, NAMESPACE_LINK_EDGE, NAMESPACE_PAGE_DAILY,
    NAMESPACE_ROBOTS_TXT, NAMESPACE_SITEMAP_URL, NAMESPACE_SITE_RUN_DAILY,
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
        let user_agent = config.user_agent.clone();
        Ok(Self {
            config,
            origin,
            fetcher: HttpFetcher::new(&user_agent),
        })
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
                offset_pos: None,
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
                "SeoCrawl discover: minimal technical crawl, no checkpoint advance"
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
            let unchanged = content_unchanged(
                self.config.skip_unchanged_content,
                checkpoint.as_ref(),
                &page.parsed.content_hash,
            );

            let technical_score = if unchanged {
                checkpoint
                    .as_ref()
                    .map(|cp| cp.technical_score)
                    .unwrap_or(page.parsed.technical_score)
            } else {
                page.parsed.technical_score
            };
            technical_scores.push(technical_score);

            let heading_outline_json: Vec<Value> = page
                .parsed
                .heading_outline
                .iter()
                .map(|h| json!({"level": h.level, "text": h.text}))
                .collect();

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
                "technical_score": technical_score,
                "structured_data_count": page.parsed.structured_data_count,
                "h1_count": page.parsed.h1_count,
                "img_count": page.parsed.img_count,
                "img_missing_alt": page.parsed.img_missing_alt,
                "heading_outline": heading_outline_json,
                "has_faq_schema": page.parsed.has_faq_schema,
            }));

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
                store_page_checkpoint(
                    ctx.as_ref(),
                    &url,
                    &PageTechnicalCheckpoint {
                        content_hash: page.parsed.content_hash.clone(),
                        last_crawl_date: crawl_date.clone(),
                        technical_score,
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

        issue_rows.extend(site_check_rows(
            &site,
            &crawl_date,
            robots_row.is_some(),
            sitemap_rows.len(),
        ));

        let page_count = page_rows.len();
        let link_count = link_rows.len();
        let issue_count = issue_rows.len();

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
            NAMESPACE_SITE_RUN_DAILY,
            &crawl_date,
            vec![site_row],
        )?;
        info!(
            site = %site,
            crawl_date = %crawl_date,
            pages = page_count,
            links = link_count,
            issues = issue_count,
            "SeoCrawl technical sync complete"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use skippr_runtime_sdk::plugins::cdc::CheckpointEnvelope;
    use skippr_runtime_sdk::plugins::{
        OffsetValidationEntry, SourcePayloadTask, SourceSyncContext,
    };
    use skippr_runtime_sdk::protocol::RuntimeOffsetMaterializationHint;
    use skippr_runtime_sdk::source_compat::ThroughputMetrics;

    use crate::checkpoint::checkpoint_key;

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
            sitemap_probe_paths: vec!["/sitemap.xml".into()],
            openai_model: "gpt-4.1-mini".into(),
            openai_structure_enabled: false,
            skip_unchanged_content: true,
            user_agent: "SkipprSeoCrawl/test".into(),
            seed_urls: Vec::new(),
        }
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

        fn store_checkpoint(&self, key: &str, envelope: &CheckpointEnvelope) -> Result<(), String> {
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
            &PageTechnicalCheckpoint {
                content_hash: "sha256:abc".into(),
                last_crawl_date: "2026-05-29".into(),
                technical_score: 0.9,
            },
        )
        .unwrap();
        let loaded = load_page_checkpoint(&ctx, url).unwrap();
        assert_eq!(loaded.content_hash, "sha256:abc");
        assert_eq!(ctx.writes.lock().unwrap()[0], checkpoint_key(url));
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
        rt.block_on(plugin.sync(ctx.clone()))
            .expect("discover sync");
        assert!(ctx.writes.lock().unwrap().is_empty());

        std::env::remove_var("SKIPPR_SEO_CRAWL_FIXTURE_DIR");
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);
    }
}
