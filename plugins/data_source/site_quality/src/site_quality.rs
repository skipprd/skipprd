use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use chrono::Utc;
use serde_json::{json, Value};
use skippr_plugin_shared_api_source::merge_crawl_progress;
use skippr_runtime_sdk::helpers::offsets::OffsetKey;
use skippr_runtime_sdk::plugins::source_contract::SourceNamespaceContract;
use skippr_runtime_sdk::plugins::{
    DataSource, SourceExecutionContract, SourceOnceContract, SourceSyncContext,
};
use skippr_runtime_sdk::protocol::SKIPPR_RUNTIME_EXECUTION_MODE_ENV;
use skippr_runtime_sdk::source_compat::{submit_payload_batches, IngestBatch};
use tracing::info;

use crate::checkpoint::{load_page_checkpoint, store_page_checkpoint, PageCheckpoint};
use crate::config::DataSourceSiteQualityPluginConfig;
use crate::issue::{map_issues, IssueThresholds};
use crate::sampling::{
    homepage_url, normalize_site_origin, resolve_url_list, resolve_url_list_async,
    HttpSitemapFetcher, StaticSitemapFetcher, UrlMode,
};
use crate::streams::{
    active_namespaces, namespace_contract, NAMESPACE_A11Y_ISSUE, NAMESPACE_CHECK_DAILY,
    NAMESPACE_LIGHTHOUSE_AUDIT, NAMESPACE_PAGE_LAB_DAILY, NAMESPACE_SITE_RUN_DAILY,
};
use crate::worker::{build_job_request, WorkerClient, WorkerJobResult};

fn runtime_is_discover_mode() -> bool {
    std::env::var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV)
        .map(|mode| mode.eq_ignore_ascii_case("discover"))
        .unwrap_or(false)
}

pub struct DataSourceSiteQualityPlugin {
    config: DataSourceSiteQualityPluginConfig,
    origin: String,
}

impl DataSourceSiteQualityPlugin {
    pub fn new(config: DataSourceSiteQualityPluginConfig) -> Result<Self, std::io::Error> {
        config.validate()?;
        let origin = normalize_site_origin(&config.site)?;
        Ok(Self { config, origin })
    }

    fn run_date(&self) -> String {
        Utc::now().format("%Y-%m-%d").to_string()
    }

    async fn resolve_urls(&self, discover: bool) -> Result<Vec<String>, std::io::Error> {
        if discover {
            return Ok(vec![homepage_url(&self.origin)]);
        }
        if std::env::var(crate::worker::FIXTURE_ENV)
            .map(|d| !d.trim().is_empty())
            .unwrap_or(false)
            && self.config.url_mode == UrlMode::UrlList
        {
            let fetcher = StaticSitemapFetcher {
                robots_txt: None,
                sitemaps: HashMap::new(),
            };
            return resolve_url_list(
                &self.origin,
                self.config.url_mode,
                &self.config.url_list,
                self.config.max_pages_per_run,
                &fetcher,
                self.config.respect_robots,
            );
        }
        if self.config.url_mode == UrlMode::UrlList {
            let fetcher = StaticSitemapFetcher {
                robots_txt: None,
                sitemaps: HashMap::new(),
            };
            return resolve_url_list(
                &self.origin,
                self.config.url_mode,
                &self.config.url_list,
                self.config.max_pages_per_run,
                &fetcher,
                self.config.respect_robots,
            );
        }
        let fetcher = HttpSitemapFetcher::new()?;
        resolve_url_list_async(
            &self.origin,
            self.config.url_mode,
            &self.config.url_list,
            self.config.max_pages_per_run,
            &fetcher,
            self.config.respect_robots,
        )
        .await
    }

    fn devices_for_run(&self, discover: bool) -> Vec<crate::config::DeviceProfile> {
        if discover {
            return self
                .config
                .devices
                .iter()
                .filter(|d| d.profile == "mobile")
                .cloned()
                .collect();
        }
        self.config.devices.clone()
    }

    fn page_lab_row(
        &self,
        run_date: &str,
        canonical_url: &str,
        device_profile: &str,
        result: &WorkerJobResult,
        content_unchanged: bool,
    ) -> Value {
        let vitals = result.web_vitals.as_ref();
        let lh = result.lighthouse.as_ref();
        let heuristics = result.mobile_heuristics.as_ref();
        let social = result.social_preview.as_ref();
        let timings = result.timings_ms.as_ref();
        json!({
            "site": self.origin,
            "canonical_url": canonical_url,
            "run_date": run_date,
            "device_profile": device_profile,
            "final_url": result.final_url,
            "status": result.status,
            "redirect_count": result.redirect_count,
            "dom_content_loaded_ms": timings.and_then(|t| t.dom_content_loaded),
            "load_ms": timings.and_then(|t| t.load),
            "fully_loaded_ms": timings.and_then(|t| t.fully_loaded),
            "lcp_ms": vitals.and_then(|v| v.lcp),
            "inp_ms": vitals.and_then(|v| v.inp),
            "cls": vitals.and_then(|v| v.cls),
            "fcp_ms": vitals.and_then(|v| v.fcp),
            "ttfb_ms": vitals.and_then(|v| v.ttfb),
            "lh_performance": lh.and_then(|l| l.performance),
            "lh_accessibility": lh.and_then(|l| l.accessibility),
            "lh_best_practices": lh.and_then(|l| l.best_practices),
            "lh_seo": lh.and_then(|l| l.seo),
            "viewport_meta_ok": heuristics.and_then(|h| h.viewport_meta_ok),
            "horizontal_scroll": heuristics.and_then(|h| h.horizontal_scroll),
            "text_too_small_count": heuristics.and_then(|h| h.text_too_small_count),
            "tap_target_issues": heuristics.and_then(|h| h.tap_target_issues),
            "social_preview_title_present": social.and_then(|s| s.title_present),
            "social_preview_description_present": social.and_then(|s| s.description_present),
            "social_preview_image_present": social.and_then(|s| s.image_present),
            "social_preview_url_present": social.and_then(|s| s.url_present),
            "social_preview_card_present": social.and_then(|s| s.card_present),
            "social_preview_card_title_present": social.and_then(|s| s.card_title_present),
            "social_preview_card_description_present": social.and_then(|s| s.card_description_present),
            "social_preview_card_image_present": social.and_then(|s| s.card_image_present),
            "social_preview_missing_fields": social.map(|s| s.missing_fields.join(",")),
            "social_preview_card_missing_fields": social.map(|s| s.card_missing_fields.join(",")),
            "social_preview_complete": social.and_then(|s| s.complete),
            "render_hash": result.render_hash,
            "content_unchanged": content_unchanged,
            "error_code": result.error.as_ref().and_then(|e| e.get("code")).and_then(|c| c.as_str()),
        })
    }

    fn a11y_rows(
        &self,
        run_date: &str,
        canonical_url: &str,
        device_profile: &str,
        result: &WorkerJobResult,
    ) -> Vec<Value> {
        let Some(violations) = &result.axe_violations else {
            return Vec::new();
        };
        violations
            .iter()
            .map(|v| {
                json!({
                    "site": self.origin,
                    "page_url": canonical_url,
                    "run_date": run_date,
                    "device_profile": device_profile,
                    "rule_id": v.id,
                    "impact": v.impact,
                    "help": v.help,
                    "nodes": v.nodes,
                })
            })
            .collect()
    }

    fn lighthouse_audit_rows(
        &self,
        run_date: &str,
        canonical_url: &str,
        device_profile: &str,
        result: &WorkerJobResult,
    ) -> Vec<Value> {
        let Some(lh) = &result.lighthouse else {
            return Vec::new();
        };
        lh.top_failing_audits
            .iter()
            .map(|audit| {
                json!({
                    "site": self.origin,
                    "page_url": canonical_url,
                    "run_date": run_date,
                    "device_profile": device_profile,
                    "audit_id": audit.id,
                    "score": audit.score,
                    "display_value": audit.display_value,
                })
            })
            .collect()
    }

    fn checkpoint_from_result(result: &WorkerJobResult) -> Option<PageCheckpoint> {
        let render_hash = result.render_hash.clone()?;
        let vitals = result.web_vitals.as_ref();
        let lh = result.lighthouse.as_ref();
        Some(PageCheckpoint {
            render_hash,
            lcp_ms: vitals.and_then(|v| v.lcp),
            inp_ms: vitals.and_then(|v| v.inp),
            cls: vitals.and_then(|v| v.cls),
            lh_performance: lh.and_then(|l| l.performance),
            lh_accessibility: lh.and_then(|l| l.accessibility),
            lh_best_practices: lh.and_then(|l| l.best_practices),
            lh_seo: lh.and_then(|l| l.seo),
            axe_summary_hash: result
                .axe_violations
                .as_ref()
                .map(|violations| axe_summary_hash(violations)),
        })
    }

    fn submit_namespace(
        &self,
        ctx: &dyn SourceSyncContext,
        namespace: &str,
        run_date: &str,
        rows: Vec<Value>,
    ) -> Result<(), std::io::Error> {
        if rows.is_empty() {
            return Ok(());
        }
        let payload = rows
            .into_iter()
            .map(|row| serde_json::to_string(&row))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| std::io::Error::other(e.to_string()))?
            .join("\n");
        let bytes = payload.len();
        let offset_key = OffsetKey::new(namespace, run_date.to_string());
        submit_payload_batches(
            ctx,
            vec![IngestBatch {
                offset_key,
                data: payload,
                bytes,
                offset_pos: None,
                source_uri: format!("site-quality://{}/lab", self.origin),
                namespace: Some(namespace.to_string()),
                cdc_rows: None,
            }],
        )?;
        Ok(())
    }
}

fn axe_summary_hash(violations: &[crate::worker::AxeViolation]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for v in violations {
        hasher.update(v.id.as_bytes());
        if let Some(impact) = &v.impact {
            hasher.update(impact.as_bytes());
        }
    }
    format!("sha256:{:x}", hasher.finalize())
}

#[async_trait]
impl DataSource for DataSourceSiteQualityPlugin {
    fn execution_contract(&self) -> SourceExecutionContract {
        SourceExecutionContract::stream(SourceOnceContract::Finite)
    }

    fn source_namespace_contracts(&self) -> Vec<SourceNamespaceContract> {
        active_namespaces(self.config.lighthouse_enabled, self.config.axe_enabled)
            .into_iter()
            .map(|ns| namespace_contract(ns))
            .inspect(|c| {
                c.validate()
                    .expect("invalid site_quality namespace contract");
            })
            .collect()
    }

    async fn sync(&mut self, ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error> {
        self.config.validate()?;
        for contract in self.source_namespace_contracts() {
            contract
                .validate()
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        }

        let discover = runtime_is_discover_mode();
        let run_date = self.run_date();
        info!(
            site = %self.origin,
            discover,
            url_mode = ?self.config.url_mode,
            max_pages = self.config.max_pages_per_run,
            lighthouse = self.config.lighthouse_enabled,
            axe = self.config.axe_enabled,
            "Site Quality sync: resolving URL sample"
        );
        let urls = self.resolve_urls(discover).await?;
        let devices = self.devices_for_run(discover);
        let job_count = urls.len().saturating_mul(devices.len());
        info!(
            url_count = urls.len(),
            device_count = devices.len(),
            job_count,
            sample_urls = ?urls,
            device_profiles = ?devices.iter().map(|d| d.profile.as_str()).collect::<Vec<_>>(),
            "Site Quality sync: URL sample ready (each job may take 30s–3min with Lighthouse)"
        );
        if discover {
            info!("Site Quality discover: homepage + mobile only; no checkpoints");
        }

        let worker = WorkerClient::new(self.config.clone())?;
        let thresholds = IssueThresholds::default();

        let mut page_lab_rows = Vec::new();
        let mut a11y_rows = Vec::new();
        let mut issue_rows = Vec::new();
        let mut lighthouse_rows = Vec::new();
        let mut pages_ok = 0u32;
        let mut pages_failed = 0u32;

        for url in &urls {
            for device in &devices {
                let prior = if discover {
                    None
                } else {
                    load_page_checkpoint(ctx.as_ref(), url, &device.profile)
                };
                let job = build_job_request(&self.config, url, device, false, prior.clone());
                let started = Instant::now();
                info!(
                    url = %url,
                    device = %device.profile,
                    job_id = %job.job_id,
                    lighthouse = job.lighthouse_enabled,
                    axe = job.axe_enabled,
                    skip_heavy = job.skip_heavy_audits,
                    "Site Quality: starting Playwright worker job"
                );
                let result = worker.run_job(&job).await?;
                info!(
                    url = %url,
                    device = %device.profile,
                    elapsed_ms = started.elapsed().as_millis(),
                    ok = result.ok,
                    status = ?result.status,
                    final_url = ?result.final_url,
                    "Site Quality: worker job finished"
                );
                worker.throttle_delay().await;

                let content_unchanged = result.skip_heavy_audits.unwrap_or_else(|| {
                    self.config.skip_heavy_when_unchanged
                        && prior
                            .as_ref()
                            .zip(result.render_hash.as_ref())
                            .is_some_and(|(cp, hash)| cp.render_hash == *hash)
                });
                page_lab_rows.push(self.page_lab_row(
                    &run_date,
                    url,
                    &device.profile,
                    &result,
                    content_unchanged,
                ));

                if self.config.axe_enabled && !content_unchanged {
                    a11y_rows.extend(self.a11y_rows(&run_date, url, &device.profile, &result));
                }
                if self.config.lighthouse_enabled && !content_unchanged {
                    lighthouse_rows.extend(self.lighthouse_audit_rows(
                        &run_date,
                        url,
                        &device.profile,
                        &result,
                    ));
                }
                issue_rows.extend(map_issues(
                    &self.origin,
                    url,
                    &run_date,
                    &device.profile,
                    &result,
                    &thresholds,
                    self.config.lighthouse_enabled,
                    self.config.axe_enabled,
                ));

                if result.ok {
                    pages_ok += 1;
                } else {
                    pages_failed += 1;
                }

                if !discover {
                    if let Some(cp) = Self::checkpoint_from_result(&result) {
                        store_page_checkpoint(ctx.as_ref(), url, &device.profile, &cp)?;
                    }
                }
            }
        }

        self.submit_namespace(
            ctx.as_ref(),
            NAMESPACE_PAGE_LAB_DAILY,
            &run_date,
            page_lab_rows,
        )?;
        if self.config.axe_enabled {
            self.submit_namespace(ctx.as_ref(), NAMESPACE_A11Y_ISSUE, &run_date, a11y_rows)?;
        }
        self.submit_namespace(ctx.as_ref(), NAMESPACE_CHECK_DAILY, &run_date, issue_rows)?;
        if self.config.lighthouse_enabled {
            self.submit_namespace(
                ctx.as_ref(),
                NAMESPACE_LIGHTHOUSE_AUDIT,
                &run_date,
                lighthouse_rows,
            )?;
        }

        let site_run = merge_crawl_progress(json!({
            "site": self.origin,
            "run_date": run_date,
            "pages_sampled": urls.len() as u32,
            "device_profiles": devices.len() as u32,
            "pages_ok": pages_ok,
            "pages_failed": pages_failed,
            "lighthouse_enabled": self.config.lighthouse_enabled,
            "axe_enabled": self.config.axe_enabled,
        }));
        self.submit_namespace(
            ctx.as_ref(),
            NAMESPACE_SITE_RUN_DAILY,
            &run_date,
            vec![site_run],
        )?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{LazyLock, Mutex};

    use super::*;
    use crate::config::DataSourceSiteQualityPluginConfig;
    use crate::sampling::UrlMode;
    use crate::worker::{build_job_request, WorkerClient, FIXTURE_ENV};
    use skippr_runtime_sdk::plugins::cdc::CheckpointEnvelope;
    use skippr_runtime_sdk::plugins::{
        OffsetValidationEntry, SourcePayloadTask, SourceSyncContext,
    };
    use skippr_runtime_sdk::protocol::RuntimeOffsetMaterializationHint;
    use skippr_runtime_sdk::source_compat::ThroughputMetrics;

    static ENV_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn test_config() -> DataSourceSiteQualityPluginConfig {
        DataSourceSiteQualityPluginConfig {
            site: "https://example.com".into(),
            url_mode: UrlMode::UrlList,
            url_list: vec!["https://example.com/".into()],
            max_pages_per_run: 5,
            max_crawl_depth: 8,
            crawl_seed_urls: vec![],
            devices: vec![crate::config::default_devices()[0].clone()],
            wait_until: "load".into(),
            navigation_timeout_ms: 5000,
            lighthouse_enabled: true,
            lighthouse_categories: vec!["performance".into()],
            axe_enabled: true,
            axe_tags: vec!["wcag2aa".into()],
            throttle: Default::default(),
            pages_per_minute: 60,
            worker_node_path: "node".into(),
            playwright_executable_path: None,
            respect_robots: true,
            skip_heavy_when_unchanged: true,
        }
    }

    #[test]
    fn discover_emits_contracts() {
        let plugin = DataSourceSiteQualityPlugin::new(test_config()).unwrap();
        let contracts = plugin.source_namespace_contracts();
        assert!(contracts
            .iter()
            .any(|c| c.namespace == NAMESPACE_PAGE_LAB_DAILY));
        assert_eq!(contracts.len(), 5);
    }

    #[tokio::test]
    async fn discover_sync_uses_fixture_without_checkpoints() {
        let _lock = env_lock();
        let fixture_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures");
        std::env::set_var(FIXTURE_ENV, fixture_dir);
        std::env::set_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV, "discover");

        let mut plugin = DataSourceSiteQualityPlugin::new(test_config()).unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        plugin.sync(ctx.clone()).await.expect("discover sync");
        assert!(
            ctx.checkpoint_stores.lock().unwrap().is_empty(),
            "discover must not persist checkpoints"
        );

        std::env::remove_var(FIXTURE_ENV);
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);
    }

    #[tokio::test]
    async fn checkpoint_skips_heavy_audits() {
        let _lock = env_lock();
        let fixture_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures");
        std::env::set_var(FIXTURE_ENV, fixture_dir);

        let cfg = test_config();
        let worker = WorkerClient::new(cfg.clone()).unwrap();
        let device = crate::config::default_devices()[0].clone();
        let prior = PageCheckpoint {
            render_hash: "sha256:fixturehash".into(),
            lcp_ms: Some(2400.0),
            inp_ms: None,
            cls: None,
            lh_performance: Some(72.0),
            lh_accessibility: Some(91.0),
            lh_best_practices: Some(88.0),
            lh_seo: Some(95.0),
            axe_summary_hash: None,
        };
        let job = build_job_request(&cfg, "https://example.com/", &device, false, Some(prior));
        let result = worker.run_job(&job).await.expect("fixture job");
        assert_eq!(result.skip_heavy_audits, Some(true));
        assert!(result.axe_violations.as_ref().is_some_and(|v| v.is_empty()));

        std::env::remove_var(FIXTURE_ENV);
    }

    #[tokio::test]
    async fn full_fixture_sync_stores_checkpoints() {
        let _lock = env_lock();
        let fixture_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures");
        std::env::set_var(FIXTURE_ENV, fixture_dir);
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);

        let mut plugin = DataSourceSiteQualityPlugin::new(test_config()).unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        plugin.sync(ctx.clone()).await.expect("sync");
        assert!(!ctx.checkpoint_stores.lock().unwrap().is_empty());

        std::env::remove_var(FIXTURE_ENV);
    }

    #[derive(Default)]
    struct RecordingSyncContext {
        checkpoint_stores: Mutex<Vec<String>>,
        checkpoints: Mutex<HashMap<String, CheckpointEnvelope>>,
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

        fn store_checkpoint(&self, key: &str, envelope: &CheckpointEnvelope) -> Result<(), String> {
            self.checkpoint_stores.lock().unwrap().push(key.to_string());
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
}
