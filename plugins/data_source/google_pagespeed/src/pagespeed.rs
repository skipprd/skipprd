use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use serde::Deserialize;
use serde_derive::Serialize;
use skippr_plugin_shared_api_source::CheckpointPayload;
use skippr_runtime_sdk::helpers::offsets::OffsetKey;
use skippr_runtime_sdk::plugins::cdc::{CheckpointAuthority, CheckpointEnvelope, CheckpointKind};
use skippr_runtime_sdk::plugins::source_contract::{
    FieldPath, SourceNamespaceContract, SourceSemantics, WritePolicy,
};
use skippr_runtime_sdk::plugins::{
    DataSource, SourceExecutionContract, SourceOnceContract, SourceSyncContext,
};
use skippr_runtime_sdk::protocol::SKIPPR_RUNTIME_EXECUTION_MODE_ENV;
use skippr_runtime_sdk::source_compat::{
    load_checkpoint_payload, submit_payload_batches, IngestBatch,
};
use tokio::time::{sleep, Instant};
use tracing::{info, warn};

use crate::client::PageSpeedClient;
use crate::config::{DataSourceGooglePageSpeedPluginConfig, Strategy};
use crate::issue::{self, API_QUOTA_EXCEEDED, INVALID_URL};
use crate::parse::{parse_error_page_row, parse_pagespeed_response, ParsedRun};
use crate::sampling::{apply_request_budget, sample_urls, SamplingInput, UrlMode};
use crate::streams::{
    NAMESPACE_AUDIT_DAILY, NAMESPACE_FIELD_ORIGIN_DAILY, NAMESPACE_ISSUE,
    NAMESPACE_PAGE_DAILY, NAMESPACE_SITE_RUN_DAILY,
};

const CHECKPOINT_PAYLOAD_VERSION: u32 = 1;

fn runtime_is_discover_mode() -> bool {
    std::env::var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV)
        .map(|mode| mode.eq_ignore_ascii_case("discover"))
        .unwrap_or(false)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RunJobCheckpoint {
    completed: Vec<CompletedJob>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
struct CompletedJob {
    canonical_url: String,
    strategy: String,
}

impl CheckpointPayload for RunJobCheckpoint {
    const VERSION: u32 = 1;
}

#[derive(Debug, Default)]
struct RunAccumulator {
    page_daily: Vec<serde_json::Value>,
    field_origin_daily: Vec<serde_json::Value>,
    audit_daily: Vec<serde_json::Value>,
    issues: Vec<serde_json::Value>,
    urls_with_field: u32,
    jobs_completed: u32,
    sampled_urls: HashSet<String>,
    mobile_perf_scores: Vec<f64>,
    errors_invalid_url: u32,
    errors_quota: u32,
    errors_other: u32,
}

pub struct DataSourceGooglePageSpeedPlugin {
    config: DataSourceGooglePageSpeedPluginConfig,
    client: PageSpeedClient,
    site_normalized: String,
}

impl DataSourceGooglePageSpeedPlugin {
    pub fn new(config: DataSourceGooglePageSpeedPluginConfig) -> Result<Self, std::io::Error> {
        config.validate()?;
        let api_key = config.resolve_api_key()?;
        let site_normalized = crate::sampling::normalize_site(&config.site)?;
        let client = PageSpeedClient::new(
            api_key,
            config.categories.clone(),
            config.locale.clone(),
        );
        Ok(Self {
            config,
            client,
            site_normalized,
        })
    }

    fn run_date() -> String {
        Utc::now().format("%Y-%m-%d").to_string()
    }

    fn checkpoint_key(&self, run_date: &str) -> String {
        format!(
            "google_pagespeed:run:{}:{}",
            run_date,
            self.site_normalized.trim_end_matches('/')
        )
    }

    fn load_completed_jobs(
        ctx: &dyn SourceSyncContext,
        key: &str,
    ) -> HashSet<CompletedJob> {
        let Some(cp) = load_checkpoint_payload::<RunJobCheckpoint>(ctx, key) else {
            return HashSet::new();
        };
        cp.completed.into_iter().collect()
    }

    fn store_completed_jobs(
        ctx: &dyn SourceSyncContext,
        key: &str,
        completed: &HashSet<CompletedJob>,
    ) -> Result<(), std::io::Error> {
        let mut jobs: Vec<CompletedJob> = completed.iter().cloned().collect();
        jobs.sort_by(|a, b| {
            a.canonical_url
                .cmp(&b.canonical_url)
                .then_with(|| a.strategy.cmp(&b.strategy))
        });
        let envelope = CheckpointEnvelope::from_payload(
            CheckpointAuthority::AdvisoryHint,
            CheckpointKind::SourceResume,
            CHECKPOINT_PAYLOAD_VERSION,
            &RunJobCheckpoint { completed: jobs },
        )
        .map_err(|e| std::io::Error::other(e.to_string()))?;
        ctx.store_checkpoint(key, &envelope)
            .map_err(std::io::Error::other)
    }

    fn namespace_contract(namespace: &str) -> SourceNamespaceContract {
        let partition_key = vec![FieldPath::single("run_date")];
        let (primary_key, description) = match namespace {
            NAMESPACE_SITE_RUN_DAILY => (
                vec![
                    FieldPath::single("site"),
                    FieldPath::single("run_date"),
                ],
                "Google PageSpeed daily site rollup",
            ),
            NAMESPACE_PAGE_DAILY => (
                vec![
                    FieldPath::single("site"),
                    FieldPath::single("run_date"),
                    FieldPath::single("canonical_url"),
                    FieldPath::single("strategy"),
                ],
                "Google PageSpeed URL × strategy daily lab + field metrics",
            ),
            NAMESPACE_FIELD_ORIGIN_DAILY => (
                vec![
                    FieldPath::single("site"),
                    FieldPath::single("run_date"),
                ],
                "Google PageSpeed origin-level CrUX field rollup",
            ),
            NAMESPACE_AUDIT_DAILY => (
                vec![
                    FieldPath::single("site"),
                    FieldPath::single("run_date"),
                    FieldPath::single("canonical_url"),
                    FieldPath::single("audit_id"),
                    FieldPath::single("strategy"),
                ],
                "Google PageSpeed top failing Lighthouse audits",
            ),
            NAMESPACE_ISSUE => (
                vec![
                    FieldPath::single("site"),
                    FieldPath::single("run_date"),
                    FieldPath::single("canonical_url"),
                    FieldPath::single("issue_code"),
                    FieldPath::single("strategy"),
                ],
                "Google PageSpeed threshold-based issues",
            ),
            _ => (
                vec![FieldPath::single("site"), FieldPath::single("run_date")],
                "Google PageSpeed namespace",
            ),
        };
        SourceNamespaceContract {
            namespace: namespace.to_string(),
            primary_key,
            cursor: Some(FieldPath::single("run_date")),
            partition_key,
            write_policy: WritePolicy::ReplacePartition,
            refresh_window: None,
            description: description.into(),
            semantics: Some(SourceSemantics::MutableReport),
        }
    }

    fn submit_namespace_rows(
        &self,
        ctx: &dyn SourceSyncContext,
        namespace: &str,
        run_date: &str,
        rows: Vec<serde_json::Value>,
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
                source_uri: format!(
                    "google-pagespeed://{}/{}",
                    self.site_normalized.trim_end_matches('/'),
                    namespace
                ),
                namespace: Some(namespace.to_string()),
                cdc_rows: None,
            }],
        )?;
        Ok(())
    }

    fn merge_parsed(&self, acc: &mut RunAccumulator, parsed: ParsedRun) {
        acc.jobs_completed += 1;
        if let Some(page) = parsed.page_daily.first().and_then(|p| p.as_object()) {
            if let Some(url) = page.get("canonical_url").and_then(|v| v.as_str()) {
                acc.sampled_urls.insert(url.to_string());
            }
            if page.get("field_data_available").and_then(|v| v.as_bool()) == Some(true) {
                acc.urls_with_field += 1;
            }
            if page.get("strategy").and_then(|v| v.as_str()) == Some("mobile") {
                if let Some(score) = page.get("lh_performance").and_then(|v| v.as_f64()) {
                    acc.mobile_perf_scores.push(score);
                }
            }
            if let Some(code) = page.get("error_code").and_then(|v| v.as_str()) {
                match code {
                    INVALID_URL => acc.errors_invalid_url += 1,
                    API_QUOTA_EXCEEDED => acc.errors_quota += 1,
                    _ => acc.errors_other += 1,
                }
            }
        }
        acc.page_daily.extend(parsed.page_daily);
        if acc.field_origin_daily.is_empty() {
            acc.field_origin_daily = parsed.field_origin_daily;
        }
        acc.audit_daily.extend(parsed.audit_daily);
        acc.issues.extend(parsed.issues);
    }

    fn site_run_daily_row(&self, run_date: &str, acc: &RunAccumulator) -> serde_json::Value {
        let urls_sampled = acc.sampled_urls.len() as u32;
        let field_pct = if acc.jobs_completed == 0 {
            0.0
        } else {
            (acc.urls_with_field as f64 / acc.jobs_completed as f64) * 100.0
        };
        let median_mobile_perf = median(&acc.mobile_perf_scores);
        serde_json::json!({
            "site": self.site_normalized,
            "run_date": run_date,
            "urls_sampled": urls_sampled,
            "api_calls_completed": acc.jobs_completed,
            "urls_with_field_data": acc.urls_with_field,
            "pct_with_field_data": field_pct,
            "median_lh_performance_mobile": median_mobile_perf,
            "errors_invalid_url": acc.errors_invalid_url,
            "errors_quota": acc.errors_quota,
            "errors_other": acc.errors_other,
        })
    }

    async fn rate_limit_wait(&self, last_request: &mut Instant) {
        let rpm = self.config.requests_per_minute.max(1);
        let min_interval = Duration::from_secs(60) / rpm;
        let elapsed = last_request.elapsed();
        if elapsed < min_interval {
            sleep(min_interval - elapsed).await;
        }
        *last_request = Instant::now();
    }

    fn classify_api_error(err: &std::io::Error) -> &'static str {
        let msg = err.to_string().to_ascii_lowercase();
        if msg.contains("http 400") || msg.contains("invalid") {
            INVALID_URL
        } else if msg.contains("429") || msg.contains("quota") {
            API_QUOTA_EXCEEDED
        } else {
            "API_ERROR"
        }
    }
}

fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 0 {
        Some((sorted[mid - 1] + sorted[mid]) / 2.0)
    } else {
        Some(sorted[mid])
    }
}

#[async_trait]
impl DataSource for DataSourceGooglePageSpeedPlugin {
    fn execution_contract(&self) -> SourceExecutionContract {
        SourceExecutionContract::stream(SourceOnceContract::Finite)
    }

    fn source_namespace_contracts(&self) -> Vec<SourceNamespaceContract> {
        let contracts: Vec<_> = [
            NAMESPACE_SITE_RUN_DAILY,
            NAMESPACE_PAGE_DAILY,
            NAMESPACE_FIELD_ORIGIN_DAILY,
            NAMESPACE_AUDIT_DAILY,
            NAMESPACE_ISSUE,
        ]
        .into_iter()
        .map(Self::namespace_contract)
        .collect();
        for contract in &contracts {
            contract
                .validate()
                .expect("invalid PageSpeed namespace contract");
        }
        contracts
    }

    async fn sync(&mut self, ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error> {
        self.config.validate()?;
        let _api_key = self.config.resolve_api_key()?;

        for contract in self.source_namespace_contracts() {
            contract
                .validate()
                .map_err(|err| std::io::Error::other(err.to_string()))?;
        }

        let discover = runtime_is_discover_mode();
        let run_date = Self::run_date();
        let checkpoint_key = self.checkpoint_key(&run_date);

        let fixture_dir = std::env::var("SKIPPR_GOOGLE_PAGESPEED_FIXTURE_DIR")
            .ok()
            .filter(|d| !d.is_empty());

        let strategies: Vec<Strategy> = if discover {
            vec![Strategy::Mobile]
        } else {
            self.config.strategies.clone()
        };

        let categories = if discover {
            vec!["performance".into()]
        } else {
            self.config.categories.clone()
        };
        let max_urls = if discover { 1 } else { self.config.max_urls };
        let urls = sample_urls(SamplingInput {
            site: &self.config.site,
            url_mode: if discover {
                UrlMode::TldSample
            } else {
                self.config.url_mode
            },
            url_list: &self.config.url_list,
            max_urls,
            respect_robots: self.config.respect_robots && !discover,
            fixture_dir: fixture_dir.as_deref(),
        })?;

        let max_requests = if discover {
            1
        } else {
            self.config.max_requests_per_run
        };
        let jobs = apply_request_budget(urls, &strategies, max_requests);

        let mut completed = if discover {
            HashSet::new()
        } else {
            Self::load_completed_jobs(ctx.as_ref(), &checkpoint_key)
        };

        let pending: Vec<_> = jobs
            .into_iter()
            .filter(|(url, strategy)| {
                !completed.contains(&CompletedJob {
                    canonical_url: url.clone(),
                    strategy: strategy.as_api_str().to_string(),
                })
            })
            .collect();

        info!(
            run_date = %run_date,
            pending_jobs = pending.len(),
            discover,
            "PageSpeed sync starting"
        );

        let mut acc = RunAccumulator::default();
        let mut last_request = Instant::now();
        let top_audits = self.config.top_audits_per_page;

        for (url, strategy) in pending {
            self.rate_limit_wait(&mut last_request).await;

            let parsed = match client.run_pagespeed(&url, &strategy).await {
                Ok(body) => parse_pagespeed_response(
                    &body,
                    &self.site_normalized,
                    &url,
                    &run_date,
                    &strategy,
                    top_audits,
                ),
                Err(err) => {
                    let code = Self::classify_api_error(&err);
                    warn!(url = %url, strategy = strategy.as_api_str(), %err, "PageSpeed API call failed");
                    parse_error_page_row(
                        &self.site_normalized,
                        &url,
                        &run_date,
                        &strategy,
                        code,
                    )
                }
            };

            self.merge_parsed(&mut acc, parsed);

            if !discover {
                completed.insert(CompletedJob {
                    canonical_url: url,
                    strategy: strategy.as_api_str().to_string(),
                });
                Self::store_completed_jobs(ctx.as_ref(), &checkpoint_key, &completed)?;
            }
        }

        let site_run = self.site_run_daily_row(&run_date, &acc);
        self.submit_namespace_rows(
            ctx.as_ref(),
            NAMESPACE_SITE_RUN_DAILY,
            &run_date,
            vec![site_run],
        )?;
        self.submit_namespace_rows(
            ctx.as_ref(),
            NAMESPACE_PAGE_DAILY,
            &run_date,
            acc.page_daily,
        )?;
        self.submit_namespace_rows(
            ctx.as_ref(),
            NAMESPACE_FIELD_ORIGIN_DAILY,
            &run_date,
            acc.field_origin_daily,
        )?;
        self.submit_namespace_rows(
            ctx.as_ref(),
            NAMESPACE_AUDIT_DAILY,
            &run_date,
            acc.audit_daily,
        )?;
        self.submit_namespace_rows(ctx.as_ref(), NAMESPACE_ISSUE, &run_date, acc.issues)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Strategy;
    use crate::sampling::apply_request_budget;
    use std::sync::Mutex;

    static ENV_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn env_test_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn test_config() -> DataSourceGooglePageSpeedPluginConfig {
        DataSourceGooglePageSpeedPluginConfig {
            site: "https://example.com".into(),
            api_key: Some("test-key".into()),
            url_mode: crate::sampling::UrlMode::UrlList,
            url_list: vec!["https://example.com/".into()],
            max_urls: 10,
            strategies: vec![Strategy::Mobile, Strategy::Desktop],
            categories: vec!["performance".into()],
            locale: "en_US".into(),
            max_requests_per_run: 10,
            requests_per_minute: 60,
            respect_robots: false,
            top_audits_per_page: 5,
            max_concurrent_requests: 1,
        }
    }

    #[test]
    fn namespace_contracts_count() {
        let plugin = DataSourceGooglePageSpeedPlugin::new(test_config()).unwrap();
        assert_eq!(plugin.source_namespace_contracts().len(), 5);
    }

    #[test]
    fn checkpoint_resume_skips_completed_pairs() {
        let _guard = env_test_lock();
        let mut completed = HashSet::new();
        completed.insert(CompletedJob {
            canonical_url: "https://example.com/".into(),
            strategy: "mobile".into(),
        });
        let jobs = apply_request_budget(
            vec!["https://example.com/".into()],
            &[Strategy::Mobile, Strategy::Desktop],
            10,
        );
        let pending: Vec<_> = jobs
            .into_iter()
            .filter(|(url, strategy)| {
                !completed.contains(&CompletedJob {
                    canonical_url: url.clone(),
                    strategy: strategy.as_api_str().to_string(),
                })
            })
            .collect();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].1, Strategy::Desktop);
    }

    #[test]
    fn run_checkpoint_roundtrip() {
        let cp = RunJobCheckpoint {
            completed: vec![CompletedJob {
                canonical_url: "https://example.com/".into(),
                strategy: "mobile".into(),
            }],
        };
        let bytes = serde_json::to_vec(&cp).unwrap();
        let decoded: RunJobCheckpoint = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded.completed.len(), 1);
    }

    #[test]
    fn config_requires_api_key_without_fixture() {
        let _guard = env_test_lock();
        std::env::remove_var("PAGESPEED_API_KEY");
        std::env::remove_var("SKIPPR_GOOGLE_PAGESPEED_FIXTURE_DIR");
        let mut cfg = test_config();
        cfg.api_key = None;
        let err = DataSourceGooglePageSpeedPlugin::new(cfg).unwrap_err();
        assert!(err.to_string().contains("PAGESPEED_API_KEY"));
    }

    #[test]
    fn fixture_dir_allows_missing_api_key() {
        let _guard = env_test_lock();
        std::env::set_var(
            "SKIPPR_GOOGLE_PAGESPEED_FIXTURE_DIR",
            concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures"),
        );
        let mut cfg = test_config();
        cfg.api_key = None;
        assert!(DataSourceGooglePageSpeedPlugin::new(cfg).is_ok());
        std::env::remove_var("SKIPPR_GOOGLE_PAGESPEED_FIXTURE_DIR");
    }
}
