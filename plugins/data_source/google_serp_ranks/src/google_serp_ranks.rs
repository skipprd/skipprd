use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use chrono::Utc;
use serde_json::{json, Value};
use skippr_runtime_sdk::helpers::offsets::OffsetKey;
use skippr_runtime_sdk::plugins::source_contract::SourceNamespaceContract;
use skippr_runtime_sdk::plugins::{
    DataSource, SourceExecutionContract, SourceOnceContract, SourceSyncContext,
};
use skippr_runtime_sdk::protocol::SKIPPR_RUNTIME_EXECUTION_MODE_ENV;
use skippr_runtime_sdk::source_compat::{submit_payload_batches, IngestBatch};
use tracing::{info, warn};

use crate::allintitle::build_allintitle_row;
use crate::checkpoint::{
    load_query_checkpoint, should_skip_query_today, store_query_checkpoint, QueryCheckpoint,
    QueryTerminalStatus,
};
use crate::config::DataSourceGoogleSerpRanksPluginConfig;
use crate::streams::{
    active_namespaces, namespace_contract, NAMESPACE_ALLINTITLE_DAILY, NAMESPACE_RESULT_DAILY,
    NAMESPACE_RUN_DAILY, NAMESPACE_TARGET_RANK_DAILY,
};
use crate::worker::{build_job_request, WorkerClient, WorkerJobResult};

fn runtime_is_discover_mode() -> bool {
    std::env::var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV)
        .map(|mode| mode.eq_ignore_ascii_case("discover"))
        .unwrap_or(false)
}

pub struct DataSourceGoogleSerpRanksPlugin {
    config: DataSourceGoogleSerpRanksPluginConfig,
    match_domains: Vec<String>,
}

impl DataSourceGoogleSerpRanksPlugin {
    pub fn new(config: DataSourceGoogleSerpRanksPluginConfig) -> Result<Self, std::io::Error> {
        config.validate()?;
        let match_domains = config.match_domains();
        Ok(Self {
            config,
            match_domains,
        })
    }

    fn run_date(&self) -> String {
        Utc::now().format("%Y-%m-%d").to_string()
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
                source_uri: format!("google-serp://{namespace}"),
                namespace: Some(namespace.to_string()),
                cdc_rows: None,
            }],
        )?;
        Ok(())
    }

    fn terminal_status(result: &WorkerJobResult) -> QueryTerminalStatus {
        if result.status == "blocked" {
            QueryTerminalStatus::Blocked
        } else if result.ok && result.status == "ok" {
            QueryTerminalStatus::Completed
        } else {
            QueryTerminalStatus::Error
        }
    }

    fn run_daily_row(
        &self,
        run_date: &str,
        keyword: &str,
        result: &WorkerJobResult,
        elapsed_ms: u128,
        skipped: bool,
    ) -> Value {
        let features = result.serp_features.clone().unwrap_or_default();
        json!({
            "run_date": run_date,
            "keyword": keyword,
            "country": self.config.country,
            "language": self.config.language,
            "device": self.config.device.as_str(),
            "status": if skipped { "skipped" } else { &result.status },
            "blocked_reason": result.blocked_reason,
            "results_inspected": result.results_inspected,
            "pages_fetched": result.pages_fetched,
            "search_url_hash": result.search_url_hash,
            "fetch_backend": "brightdata",
            "elapsed_ms": elapsed_ms,
            "skipped_same_day": skipped,
            "worker_ok": result.ok,
            "error_code": result.error.as_ref().and_then(|e| e.get("code")).and_then(|c| c.as_str()),
            "has_ai_overview": features.has_ai_overview,
            "has_paa": features.has_paa,
            "has_video": features.has_video,
            "has_sitelinks": features.has_sitelinks,
            "has_featured_snippet": features.has_featured_snippet,
            "owns_featured_snippet": features.owns_featured_snippet,
            "has_local_pack": features.has_local_pack,
            "has_shopping": features.has_shopping,
            "has_images": features.has_images,
            "has_knowledge_graph": features.has_knowledge_graph,
            "has_answer_box": features.has_answer_box,
            "has_related_searches": features.has_related_searches,
        })
    }

    fn target_rank_rows(
        &self,
        run_date: &str,
        keyword: &str,
        result: &WorkerJobResult,
    ) -> Vec<Value> {
        result
            .target_matches
            .iter()
            .map(|m| {
                json!({
                    "run_date": run_date,
                    "keyword": keyword,
                    "country": self.config.country,
                    "language": self.config.language,
                    "device": self.config.device.as_str(),
                    "target_site": m.target_site,
                    "matched_url": m.matched_url,
                    "matched_domain": m.matched_domain,
                    "position": m.position,
                    "page_start": m.page_start,
                    "found": m.found,
                    "search_url_hash": result.search_url_hash,
                })
            })
            .collect()
    }

    fn result_daily_rows(
        &self,
        run_date: &str,
        keyword: &str,
        result: &WorkerJobResult,
    ) -> Vec<Value> {
        result
            .organic_results
            .iter()
            .map(|r| {
                json!({
                    "run_date": run_date,
                    "keyword": keyword,
                    "country": self.config.country,
                    "language": self.config.language,
                    "device": self.config.device.as_str(),
                    "position": r.position,
                    "title": r.title,
                    "url": r.url,
                    "domain": r.domain,
                    "snippet": r.snippet,
                    "page_start": r.page_start,
                    "search_url_hash": result.search_url_hash,
                })
            })
            .collect()
    }

    async fn sync_allintitle(
        &self,
        ctx: &dyn SourceSyncContext,
        worker: &WorkerClient,
        run_date: &str,
    ) -> Result<(), std::io::Error> {
        let site = self.config.primary_site();
        let keywords = self.config.allintitle_keywords_for_run();
        let mut rows = Vec::new();
        for keyword in &keywords {
            info!(keyword = %keyword, "Google SERP allintitle: fetching results count");
            let count = match worker.fetch_allintitle_count(keyword).await {
                Ok(count) => count,
                Err(err) => {
                    warn!(keyword = %keyword, error = %err, "Google SERP allintitle: fetch failed");
                    None
                }
            };
            rows.push(build_allintitle_row(
                &site,
                run_date,
                keyword,
                count,
                &self.config.country,
                &self.config.language,
                self.config.device.as_str(),
            ));
            worker.throttle_delay().await;
        }
        self.submit_namespace(ctx, NAMESPACE_ALLINTITLE_DAILY, run_date, rows)
    }

    async fn sync_organic(
        &self,
        ctx: &dyn SourceSyncContext,
        discover: bool,
        run_date: &str,
        worker: &WorkerClient,
    ) -> Result<(), std::io::Error> {
        let keywords = self.config.queries_for_run(discover);
        let max_depth = self.config.effective_max_depth(discover);
        let targets = if discover {
            self.match_domains
                .first()
                .map(|d| vec![d.clone()])
                .unwrap_or_default()
        } else {
            self.match_domains.clone()
        };

        if discover {
            info!(
                run_date = %run_date,
                max_depth,
                "Google SERP discover: one keyword, first target only, no checkpoints"
            );
        }

        let mut run_rows = Vec::new();
        let mut target_rows = Vec::new();
        let mut result_rows = Vec::new();
        let mut queries_run = 0u32;

        for keyword in &keywords {
            let prior = if discover {
                None
            } else {
                load_query_checkpoint(
                    ctx,
                    keyword,
                    &self.config.country,
                    &self.config.language,
                    self.config.device.as_str(),
                )
            };
            if !discover
                && should_skip_query_today(
                    prior.as_ref(),
                    run_date,
                    self.config.force_refresh_today,
                )
            {
                info!(keyword = %keyword, "Google SERP: skipping query already checked today");
                run_rows.push(self.run_daily_row(
                    run_date,
                    keyword,
                    &WorkerJobResult {
                        job_id: String::new(),
                        ok: true,
                        status: "skipped".into(),
                        blocked_reason: None,
                        organic_results: vec![],
                        target_matches: vec![],
                        results_inspected: 0,
                        pages_fetched: 0,
                        search_url_hash: None,
                        serp_features: None,
                        error: None,
                    },
                    0,
                    true,
                ));
                continue;
            }

            let job = build_job_request(
                &self.config,
                keyword,
                max_depth,
                targets.clone(),
                prior.as_ref(),
            );
            let started = Instant::now();
            info!(
                keyword = %keyword,
                job_id = %job.job_id,
                max_depth,
                target_count = targets.len(),
                "Google SERP: starting query job"
            );
            let result = worker.run_job(&job).await?;
            let elapsed_ms = started.elapsed().as_millis();
            info!(
                keyword = %keyword,
                elapsed_ms,
                status = %result.status,
                blocked_reason = ?result.blocked_reason,
                results_inspected = result.results_inspected,
                "Google SERP: query job finished"
            );

            run_rows.push(self.run_daily_row(run_date, keyword, &result, elapsed_ms, false));
            target_rows.extend(self.target_rank_rows(run_date, keyword, &result));
            if self.config.capture_results {
                result_rows.extend(self.result_daily_rows(run_date, keyword, &result));
            }

            queries_run += 1;

            if !discover {
                let best_position = result
                    .target_matches
                    .iter()
                    .filter_map(|row| row.position)
                    .min();
                let best_page_start = result
                    .target_matches
                    .iter()
                    .filter(|row| row.found)
                    .filter_map(|row| row.page_start)
                    .min();
                let checkpoint = QueryCheckpoint {
                    run_date: run_date.to_string(),
                    status: Self::terminal_status(&result),
                    last_position: best_position,
                    last_page_start: best_page_start,
                };
                store_query_checkpoint(
                    ctx,
                    keyword,
                    &self.config.country,
                    &self.config.language,
                    self.config.device.as_str(),
                    &checkpoint,
                )?;
            }

            if result.status == "blocked" {
                warn!(
                    keyword = %keyword,
                    reason = ?result.blocked_reason,
                    "Google SERP: blocked page detected; not retrying in this run"
                );
            }

            worker.throttle_delay().await;
        }

        self.submit_namespace(ctx, NAMESPACE_RUN_DAILY, run_date, run_rows)?;
        self.submit_namespace(ctx, NAMESPACE_TARGET_RANK_DAILY, run_date, target_rows)?;
        if self.config.capture_results {
            self.submit_namespace(ctx, NAMESPACE_RESULT_DAILY, run_date, result_rows)?;
        }

        info!(
            run_date = %run_date,
            queries_run,
            keywords_total = keywords.len(),
            "Google SERP organic sync complete"
        );
        Ok(())
    }
}

#[async_trait]
impl DataSource for DataSourceGoogleSerpRanksPlugin {
    fn execution_contract(&self) -> SourceExecutionContract {
        SourceExecutionContract::stream(SourceOnceContract::Finite)
    }

    fn source_namespace_contracts(&self) -> Vec<SourceNamespaceContract> {
        active_namespaces(self.config.capture_results, self.config.include_allintitle)
            .into_iter()
            .map(|ns| namespace_contract(ns))
            .inspect(|c| {
                c.validate()
                    .expect("invalid google_serp_ranks namespace contract");
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
        let worker = WorkerClient::new(self.config.clone())?;

        if !self.config.allintitle_only {
            self.sync_organic(ctx.as_ref(), discover, &run_date, &worker)
                .await?;
        }

        if self.config.include_allintitle {
            self.sync_allintitle(ctx.as_ref(), &worker, &run_date)
                .await?;
        }

        info!(run_date = %run_date, "Google SERP sync complete");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! Path matrix covered by tests below:
    //!
    //! **Happy**
    //! - Valid config passes validation; domains deduped from aliases.
    //! - Sync (ok fixture): run_daily + target_rank rows; target found with position.
    //! - `capture_results: true` emits `result_daily`; false omits it.
    //! - Discover: one keyword, no checkpoint, namespaces still declared.
    //! - Same-day checkpoint skips worker (status `skipped`).
    //! - `force_refresh_today` re-runs despite checkpoint.
    //! - `max_queries_per_run` caps keyword batch size.
    //! - Checkpoint roundtrip stores `Completed` after ok sync.
    //!
    //! **Unhappy**
    //! - Invalid config (empty targets/keywords, volume cap violations).
    //! - Blocked fixture: `blocked` status, `Blocked` checkpoint, no target rank rows.
    //! - Not-found fixture: `ok` with `found: false` target row.
    //! - Error fixture: `error` status, `Error` checkpoint.
    //! - Worker parse: empty line / invalid JSON.
    //! - Fixture missing for keyword → `NotFound`.

    use std::sync::{Arc, LazyLock, Mutex};

    use super::*;
    use crate::checkpoint::{
        load_query_checkpoint, store_query_checkpoint, QueryCheckpoint, QueryTerminalStatus,
    };
    use crate::streams::{
        NAMESPACE_ALLINTITLE_DAILY, NAMESPACE_RESULT_DAILY, NAMESPACE_RUN_DAILY,
        NAMESPACE_TARGET_RANK_DAILY,
    };
    use crate::test_support::{
        clear_fixture_dir, sample_config, set_fixture_dir, RecordingSyncContext,
    };
    use chrono::Utc;
    use skippr_runtime_sdk::protocol::SKIPPR_RUNTIME_EXECUTION_MODE_ENV;

    static ENV_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn run_date_today() -> String {
        Utc::now().format("%Y-%m-%d").to_string()
    }

    #[test]
    fn namespace_contract_count_with_capture_results() {
        let mut cfg = sample_config();
        cfg.capture_results = true;
        let plugin = DataSourceGoogleSerpRanksPlugin::new(cfg).unwrap();
        assert_eq!(plugin.source_namespace_contracts().len(), 3);
    }

    #[test]
    fn terminal_status_maps_worker_outcomes() {
        assert!(matches!(
            DataSourceGoogleSerpRanksPlugin::terminal_status(&WorkerJobResult {
                job_id: "j".into(),
                ok: true,
                status: "ok".into(),
                blocked_reason: None,
                organic_results: vec![],
                target_matches: vec![],
                results_inspected: 0,
                pages_fetched: 0,
                search_url_hash: None,
                serp_features: None,
                error: None,
            }),
            QueryTerminalStatus::Completed
        ));
        assert!(matches!(
            DataSourceGoogleSerpRanksPlugin::terminal_status(&WorkerJobResult {
                job_id: "j".into(),
                ok: true,
                status: "blocked".into(),
                blocked_reason: Some("captcha".into()),
                organic_results: vec![],
                target_matches: vec![],
                results_inspected: 0,
                pages_fetched: 1,
                search_url_hash: None,
                serp_features: None,
                error: None,
            }),
            QueryTerminalStatus::Blocked
        ));
        assert!(matches!(
            DataSourceGoogleSerpRanksPlugin::terminal_status(&WorkerJobResult {
                job_id: "j".into(),
                ok: false,
                status: "error".into(),
                blocked_reason: None,
                organic_results: vec![],
                target_matches: vec![],
                results_inspected: 0,
                pages_fetched: 0,
                search_url_hash: None,
                serp_features: None,
                error: Some(serde_json::json!({"code": "NAVIGATION_ERROR"})),
            }),
            QueryTerminalStatus::Error
        ));
    }

    #[tokio::test]
    async fn happy_allintitle_only_emits_allintitle_rows() {
        let _guard = env_lock();
        set_fixture_dir();
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);

        let mut cfg = sample_config();
        cfg.allintitle_only = true;
        cfg.include_allintitle = true;
        cfg.allintitle_keywords = vec!["meal planning app free".into()];
        let mut plugin = DataSourceGoogleSerpRanksPlugin::new(cfg).unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        plugin.sync(ctx.clone()).await.expect("sync");

        assert!(ctx.rows_for_namespace(NAMESPACE_RUN_DAILY).is_empty());
        let allintitle = ctx.rows_for_namespace(NAMESPACE_ALLINTITLE_DAILY);
        assert_eq!(allintitle.len(), 1);
        assert_eq!(allintitle[0]["allintitle_count"], 42);
        assert_eq!(allintitle[0]["query_status"], "ok");

        clear_fixture_dir();
    }

    #[tokio::test]
    async fn happy_sync_emits_run_and_target_rows() {
        let _guard = env_lock();
        set_fixture_dir();
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);

        let mut cfg = sample_config();
        cfg.keywords = vec!["fixture keyword".into()];
        cfg.capture_results = true;
        let mut plugin = DataSourceGoogleSerpRanksPlugin::new(cfg).unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        plugin.sync(ctx.clone()).await.expect("sync");

        let runs = ctx.rows_for_namespace(NAMESPACE_RUN_DAILY);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0]["status"], "ok");
        assert_eq!(runs[0]["skipped_same_day"], false);
        assert_eq!(runs[0]["results_inspected"], 10);

        let targets = ctx.rows_for_namespace(NAMESPACE_TARGET_RANK_DAILY);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0]["found"], true);
        assert_eq!(targets[0]["position"], 3);

        assert_eq!(ctx.rows_for_namespace(NAMESPACE_RESULT_DAILY).len(), 1);
        let results = ctx.rows_for_namespace(NAMESPACE_RESULT_DAILY);
        assert_eq!(results[0]["domain"], "example.com");
        assert!(ctx
            .submitted_namespaces()
            .iter()
            .any(|ns| ns == NAMESPACE_RESULT_DAILY));

        clear_fixture_dir();
    }

    #[tokio::test]
    async fn happy_sync_without_capture_omits_result_namespace() {
        let _guard = env_lock();
        set_fixture_dir();
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);

        let mut cfg = sample_config();
        cfg.capture_results = false;
        let mut plugin = DataSourceGoogleSerpRanksPlugin::new(cfg).unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        plugin.sync(ctx.clone()).await.expect("sync");

        assert!(!ctx
            .submitted_namespaces()
            .iter()
            .any(|ns| ns == NAMESPACE_RESULT_DAILY));

        clear_fixture_dir();
    }

    #[tokio::test]
    async fn happy_discover_skips_checkpoints_and_limits_keywords() {
        let _guard = env_lock();
        set_fixture_dir();
        std::env::set_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV, "discover");

        let mut cfg = sample_config();
        cfg.keywords = vec!["fixture keyword".into(), "second".into(), "third".into()];
        let mut plugin = DataSourceGoogleSerpRanksPlugin::new(cfg).unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        plugin.sync(ctx.clone()).await.expect("discover sync");

        assert!(ctx.checkpoint_stores.lock().unwrap().is_empty());
        assert_eq!(ctx.rows_for_namespace(NAMESPACE_RUN_DAILY).len(), 1);

        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);
        clear_fixture_dir();
    }

    #[tokio::test]
    async fn happy_same_day_checkpoint_skips_worker() {
        let _guard = env_lock();
        set_fixture_dir();
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);

        let cfg = sample_config();
        let ctx = Arc::new(RecordingSyncContext::default());
        let run_date = run_date_today();
        store_query_checkpoint(
            ctx.as_ref(),
            "fixture keyword",
            &cfg.country,
            &cfg.language,
            cfg.device.as_str(),
            &QueryCheckpoint {
                run_date: run_date.clone(),
                status: QueryTerminalStatus::Completed,
                last_position: None,
                last_page_start: None,
            },
        )
        .expect("seed checkpoint");

        let mut plugin = DataSourceGoogleSerpRanksPlugin::new(cfg).unwrap();
        plugin.sync(ctx.clone()).await.expect("sync");

        let runs = ctx.rows_for_namespace(NAMESPACE_RUN_DAILY);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0]["status"], "skipped");
        assert_eq!(runs[0]["skipped_same_day"], true);
        assert_eq!(ctx.checkpoint_stores.lock().unwrap().len(), 1);

        clear_fixture_dir();
    }

    #[tokio::test]
    async fn happy_force_refresh_runs_despite_checkpoint() {
        let _guard = env_lock();
        set_fixture_dir();
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);

        let mut cfg = sample_config();
        cfg.force_refresh_today = true;
        let ctx = Arc::new(RecordingSyncContext::default());
        store_query_checkpoint(
            ctx.as_ref(),
            "fixture keyword",
            &cfg.country,
            &cfg.language,
            cfg.device.as_str(),
            &QueryCheckpoint {
                run_date: run_date_today(),
                status: QueryTerminalStatus::Completed,
                last_position: None,
                last_page_start: None,
            },
        )
        .expect("seed checkpoint");

        let mut plugin = DataSourceGoogleSerpRanksPlugin::new(cfg).unwrap();
        plugin.sync(ctx.clone()).await.expect("sync");

        let runs = ctx.rows_for_namespace(NAMESPACE_RUN_DAILY);
        assert_eq!(runs[0]["status"], "ok");
        assert_eq!(ctx.checkpoint_stores.lock().unwrap().len(), 2);

        clear_fixture_dir();
    }

    #[tokio::test]
    async fn happy_max_queries_per_run_caps_keywords() {
        let _guard = env_lock();
        set_fixture_dir();
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);

        let mut cfg = sample_config();
        cfg.keywords = vec!["fixture keyword".into(), "second".into(), "third".into()];
        cfg.max_queries_per_run = 2;
        let mut plugin = DataSourceGoogleSerpRanksPlugin::new(cfg).unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        plugin.sync(ctx.clone()).await.expect("sync");

        assert_eq!(ctx.rows_for_namespace(NAMESPACE_RUN_DAILY).len(), 2);
        assert_eq!(ctx.checkpoint_stores.lock().unwrap().len(), 2);

        clear_fixture_dir();
    }

    #[tokio::test]
    async fn unhappy_blocked_sync_records_blocked_checkpoint() {
        let _guard = env_lock();
        set_fixture_dir();
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);

        let mut cfg = sample_config();
        cfg.keywords = vec!["blocked query".into()];
        let mut plugin = DataSourceGoogleSerpRanksPlugin::new(cfg.clone()).unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        plugin.sync(ctx.clone()).await.expect("sync");

        let runs = ctx.rows_for_namespace(NAMESPACE_RUN_DAILY);
        assert_eq!(runs[0]["status"], "blocked");
        assert_eq!(runs[0]["blocked_reason"], "captcha");
        assert!(ctx
            .rows_for_namespace(NAMESPACE_TARGET_RANK_DAILY)
            .is_empty());

        let cp = load_query_checkpoint(
            ctx.as_ref(),
            "blocked query",
            &cfg.country,
            &cfg.language,
            cfg.device.as_str(),
        )
        .expect("checkpoint");
        assert_eq!(cp.status, QueryTerminalStatus::Blocked);

        clear_fixture_dir();
    }

    #[tokio::test]
    async fn unhappy_not_found_emits_absent_target_row() {
        let _guard = env_lock();
        set_fixture_dir();
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);

        let mut cfg = sample_config();
        cfg.keywords = vec!["not found query".into()];
        let mut plugin = DataSourceGoogleSerpRanksPlugin::new(cfg).unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        plugin.sync(ctx.clone()).await.expect("sync");

        let targets = ctx.rows_for_namespace(NAMESPACE_TARGET_RANK_DAILY);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0]["found"], false);
        assert!(targets[0]["position"].is_null());

        clear_fixture_dir();
    }

    #[tokio::test]
    async fn unhappy_error_sync_records_error_checkpoint() {
        let _guard = env_lock();
        set_fixture_dir();
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);

        let mut cfg = sample_config();
        cfg.keywords = vec!["navigation error".into()];
        let mut plugin = DataSourceGoogleSerpRanksPlugin::new(cfg.clone()).unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        plugin.sync(ctx.clone()).await.expect("sync");

        let runs = ctx.rows_for_namespace(NAMESPACE_RUN_DAILY);
        assert_eq!(runs[0]["status"], "error");
        assert_eq!(runs[0]["worker_ok"], false);

        let cp = load_query_checkpoint(
            ctx.as_ref(),
            "navigation error",
            &cfg.country,
            &cfg.language,
            cfg.device.as_str(),
        )
        .expect("checkpoint");
        assert_eq!(cp.status, QueryTerminalStatus::Error);

        clear_fixture_dir();
    }

    #[test]
    fn unhappy_plugin_new_rejects_invalid_config() {
        let mut cfg = sample_config();
        cfg.targets.clear();
        assert!(DataSourceGoogleSerpRanksPlugin::new(cfg).is_err());
    }
}
