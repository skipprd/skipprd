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

use crate::checkpoint::{
    load_query_checkpoint, should_skip_query_today, store_query_checkpoint, QueryCheckpoint,
    QueryTerminalStatus,
};
use crate::config::DataSourceAppleAppStoreSerpPluginConfig;
use crate::itunes::{build_search_job, ItunesClient, ItunesSearchResult};
use crate::streams::{
    active_namespaces, namespace_contract, NAMESPACE_RESULT_DAILY, NAMESPACE_RUN_DAILY,
    NAMESPACE_TARGET_RANK_DAILY,
};

fn runtime_is_discover_mode() -> bool {
    std::env::var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV)
        .map(|mode| mode.eq_ignore_ascii_case("discover"))
        .unwrap_or(false)
}

pub struct DataSourceAppleAppStoreSerpPlugin {
    config: DataSourceAppleAppStoreSerpPluginConfig,
}

impl DataSourceAppleAppStoreSerpPlugin {
    pub fn new(config: DataSourceAppleAppStoreSerpPluginConfig) -> Result<Self, std::io::Error> {
        config.validate()?;
        Ok(Self { config })
    }

    fn run_date(&self) -> String {
        Utc::now().format("%Y-%m-%d").to_string()
    }

    fn entity_str(&self) -> String {
        self.config.entity.as_str().to_string()
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
                source_uri: format!("apple-app-store://{namespace}"),
                namespace: Some(namespace.to_string()),
                cdc_rows: None,
            }],
        )?;
        Ok(())
    }

    fn terminal_status(result: &ItunesSearchResult) -> QueryTerminalStatus {
        if result.ok && result.status == "ok" {
            QueryTerminalStatus::Completed
        } else {
            QueryTerminalStatus::Error
        }
    }

    fn run_daily_row(
        &self,
        run_date: &str,
        keyword: &str,
        storefront: &str,
        result: &ItunesSearchResult,
        elapsed_ms: u128,
        skipped: bool,
    ) -> Value {
        json!({
            "run_date": run_date,
            "keyword": keyword,
            "storefront": storefront,
            "entity": self.entity_str(),
            "status": if skipped { "skipped" } else { &result.status },
            "results_inspected": result.results_inspected,
            "search_url_hash": result.search_url_hash,
            "fetch_backend": "itunes",
            "elapsed_ms": elapsed_ms,
            "skipped_same_day": skipped,
            "search_ok": result.ok,
            "error_code": result.error.as_ref().and_then(|e| e.get("code")).and_then(|c| c.as_str()),
        })
    }

    fn target_rank_rows(
        &self,
        run_date: &str,
        keyword: &str,
        storefront: &str,
        result: &ItunesSearchResult,
    ) -> Vec<Value> {
        result
            .target_matches
            .iter()
            .map(|m| {
                json!({
                    "run_date": run_date,
                    "keyword": keyword,
                    "storefront": storefront,
                    "entity": self.entity_str(),
                    "target_app_id": m.target_app_id,
                    "matched_app_id": m.matched_app_id,
                    "matched_bundle_id": m.matched_bundle_id,
                    "matched_track_name": m.matched_track_name,
                    "position": m.position,
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
        storefront: &str,
        result: &ItunesSearchResult,
    ) -> Vec<Value> {
        result
            .results
            .iter()
            .map(|r| {
                json!({
                    "run_date": run_date,
                    "keyword": keyword,
                    "storefront": storefront,
                    "entity": self.entity_str(),
                    "position": r.position,
                    "app_id": r.app_id,
                    "bundle_id": r.bundle_id,
                    "track_name": r.track_name,
                    "artist_name": r.artist_name,
                    "search_url_hash": result.search_url_hash,
                })
            })
            .collect()
    }
}

#[async_trait]
impl DataSource for DataSourceAppleAppStoreSerpPlugin {
    fn execution_contract(&self) -> SourceExecutionContract {
        SourceExecutionContract::stream(SourceOnceContract::Finite)
    }

    fn source_namespace_contracts(&self) -> Vec<SourceNamespaceContract> {
        active_namespaces(self.config.capture_results)
            .into_iter()
            .map(|ns| namespace_contract(ns))
            .inspect(|c| {
                c.validate()
                    .expect("invalid apple_app_store_serp namespace contract");
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
        let pairs = self.config.pairs_for_run(discover);
        let max_depth = self.config.effective_max_depth(discover);
        let entity = self.entity_str();

        if discover {
            info!(
                run_date = %run_date,
                max_depth,
                "App Store SERP discover: one keyword×storefront pair, no checkpoints"
            );
        }

        let client = ItunesClient::new(self.config.clone())?;
        let mut run_rows = Vec::new();
        let mut target_rows = Vec::new();
        let mut result_rows = Vec::new();
        let mut queries_run = 0u32;

        for pair in &pairs {
            if !discover {
                let prior =
                    load_query_checkpoint(ctx.as_ref(), &pair.keyword, &pair.storefront, &entity);
                if should_skip_query_today(
                    prior.as_ref(),
                    &run_date,
                    self.config.force_refresh_today,
                ) {
                    info!(
                        keyword = %pair.keyword,
                        storefront = %pair.storefront,
                        "App Store SERP: skipping pair already checked today"
                    );
                    run_rows.push(self.run_daily_row(
                        &run_date,
                        &pair.keyword,
                        &pair.storefront,
                        &ItunesSearchResult {
                            job_id: String::new(),
                            ok: true,
                            status: "skipped".into(),
                            results: vec![],
                            target_matches: vec![],
                            results_inspected: 0,
                            search_url_hash: None,
                            error: None,
                        },
                        0,
                        true,
                    ));
                    continue;
                }
            }

            let job = build_search_job(&self.config, &pair.keyword, &pair.storefront, max_depth);
            let started = Instant::now();
            info!(
                keyword = %pair.keyword,
                storefront = %pair.storefront,
                job_id = %job.job_id,
                max_depth,
                "App Store SERP: starting search"
            );
            let result = client.run_search(&job).await?;
            let elapsed_ms = started.elapsed().as_millis();
            info!(
                keyword = %pair.keyword,
                storefront = %pair.storefront,
                elapsed_ms,
                status = %result.status,
                results_inspected = result.results_inspected,
                "App Store SERP: search finished"
            );

            run_rows.push(self.run_daily_row(
                &run_date,
                &pair.keyword,
                &pair.storefront,
                &result,
                elapsed_ms,
                false,
            ));
            target_rows.extend(self.target_rank_rows(
                &run_date,
                &pair.keyword,
                &pair.storefront,
                &result,
            ));
            if self.config.capture_results {
                result_rows.extend(self.result_daily_rows(
                    &run_date,
                    &pair.keyword,
                    &pair.storefront,
                    &result,
                ));
            }

            queries_run += 1;

            if !discover {
                let checkpoint = QueryCheckpoint {
                    run_date: run_date.clone(),
                    status: Self::terminal_status(&result),
                };
                store_query_checkpoint(
                    ctx.as_ref(),
                    &pair.keyword,
                    &pair.storefront,
                    &entity,
                    &checkpoint,
                )?;
            }

            if result.status == "error" {
                warn!(
                    keyword = %pair.keyword,
                    storefront = %pair.storefront,
                    error = ?result.error,
                    "App Store SERP: search error; not retrying in this run"
                );
            }

            client.throttle_delay().await;
        }

        self.submit_namespace(ctx.as_ref(), NAMESPACE_RUN_DAILY, &run_date, run_rows)?;
        self.submit_namespace(
            ctx.as_ref(),
            NAMESPACE_TARGET_RANK_DAILY,
            &run_date,
            target_rows,
        )?;
        if self.config.capture_results {
            self.submit_namespace(ctx.as_ref(), NAMESPACE_RESULT_DAILY, &run_date, result_rows)?;
        }

        info!(
            run_date = %run_date,
            queries_run,
            pairs_total = pairs.len(),
            "App Store SERP sync complete"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, LazyLock, Mutex};

    use super::*;
    use crate::checkpoint::{
        load_query_checkpoint, store_query_checkpoint, QueryCheckpoint, QueryTerminalStatus,
    };
    use crate::config::AppStoreEntity;
    use crate::streams::{
        NAMESPACE_RESULT_DAILY, NAMESPACE_RUN_DAILY, NAMESPACE_TARGET_RANK_DAILY,
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

    #[tokio::test]
    async fn happy_sync_emits_run_and_target_rows() {
        let _guard = env_lock();
        set_fixture_dir();
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);

        let mut cfg = sample_config();
        cfg.keywords = vec!["fixture keyword".into()];
        cfg.capture_results = true;
        let mut plugin = DataSourceAppleAppStoreSerpPlugin::new(cfg).unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        plugin.sync(ctx.clone()).await.expect("sync");

        let runs = ctx.rows_for_namespace(NAMESPACE_RUN_DAILY);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0]["status"], "ok");
        assert_eq!(runs[0]["fetch_backend"], "itunes");

        let targets = ctx.rows_for_namespace(NAMESPACE_TARGET_RANK_DAILY);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0]["found"], true);
        assert_eq!(targets[0]["position"], 3);

        assert_eq!(ctx.rows_for_namespace(NAMESPACE_RESULT_DAILY).len(), 1);

        clear_fixture_dir();
    }

    #[tokio::test]
    async fn happy_same_day_checkpoint_skips_search() {
        let _guard = env_lock();
        set_fixture_dir();
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);

        let cfg = sample_config();
        let ctx = Arc::new(RecordingSyncContext::default());
        let entity = AppStoreEntity::Software.as_str();
        store_query_checkpoint(
            ctx.as_ref(),
            "fixture keyword",
            "us",
            entity,
            &QueryCheckpoint {
                run_date: run_date_today(),
                status: QueryTerminalStatus::Completed,
            },
        )
        .expect("seed checkpoint");

        let mut plugin = DataSourceAppleAppStoreSerpPlugin::new(cfg).unwrap();
        plugin.sync(ctx.clone()).await.expect("sync");

        let runs = ctx.rows_for_namespace(NAMESPACE_RUN_DAILY);
        assert_eq!(runs[0]["status"], "skipped");

        clear_fixture_dir();
    }

    #[tokio::test]
    async fn unhappy_error_sync_records_error_checkpoint() {
        let _guard = env_lock();
        set_fixture_dir();
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);

        let mut cfg = sample_config();
        cfg.keywords = vec!["http error".into()];
        cfg.storefronts = vec!["us".into()];
        cfg.max_queries_per_run = 1;
        let mut plugin = DataSourceAppleAppStoreSerpPlugin::new(cfg.clone()).unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        plugin.sync(ctx.clone()).await.expect("sync");

        let runs = ctx.rows_for_namespace(NAMESPACE_RUN_DAILY);
        assert_eq!(runs[0]["status"], "error");

        let cp = load_query_checkpoint(ctx.as_ref(), "http error", "us", "software").expect("cp");
        assert_eq!(cp.status, QueryTerminalStatus::Error);

        clear_fixture_dir();
    }
}
