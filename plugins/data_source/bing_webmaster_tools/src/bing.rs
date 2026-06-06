use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{Duration, NaiveDate, Utc};
use serde::Deserialize;
use serde_derive::Serialize;
use skippr_plugin_shared_api_source::{
    DateWindowPlanner, OAuth2RefreshTokenAuth, RetryConfig, RetryableHttpClient,
};
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
use tokio::time::{sleep, Duration as TokioDuration};
use tracing::info;

use crate::bing_api::{
    authorization_header, bronze_rows_for_stream, fetch_api_rows, is_forbidden_site_error,
    normalize_site_url, AuthMode, DEFAULT_BING_OAUTH_TOKEN_URL,
};
use crate::streams::{
    resolve_streams, BingApiMethod, BingStreamDef, BingStreamKind, StreamProfile,
};

const CHECKPOINT_PAYLOAD_VERSION: u32 = 1;
const DISCOVER_SAMPLE_DAYS: u32 = 3;
/// Bing typically exposes roughly three months of history.
const BING_HISTORY_MONTHS: i64 = 3;

fn runtime_is_discover_mode() -> bool {
    std::env::var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV)
        .map(|mode| mode.eq_ignore_ascii_case("discover"))
        .unwrap_or(false)
}

fn discover_sample_date_window(end_date: NaiveDate) -> (NaiveDate, NaiveDate) {
    let span = DISCOVER_SAMPLE_DAYS.max(1);
    let start = end_date - Duration::days(i64::from(span - 1));
    (start, end_date)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct BingNamespaceCheckpoint {
    last_completed_date: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DataSourceBingWebmasterToolsPluginConfig {
    pub site_url: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub access_token: Option<String>,
    #[serde(default)]
    pub oauth_token_url: Option<String>,
    #[serde(default)]
    pub oauth_client_id: Option<String>,
    #[serde(default)]
    pub oauth_client_secret: Option<String>,
    #[serde(default)]
    pub oauth_refresh_token: Option<String>,
    pub start_date: String,
    #[serde(default)]
    pub end_date: Option<String>,
    #[serde(default = "default_lookback_days")]
    pub lookback_days: u32,
    #[serde(default = "default_stream_profile")]
    pub stream_profile: StreamProfile,
    #[serde(default = "default_processing_lag_days")]
    pub processing_lag_days: u32,
    #[serde(default = "default_window_in_days")]
    pub window_in_days: u32,
    #[serde(default)]
    pub streams: Option<Vec<String>>,
    #[serde(default = "default_request_interval_ms")]
    pub request_interval_ms: u64,
    #[serde(default = "default_max_api_retries")]
    pub max_api_retries: u32,
}

fn default_lookback_days() -> u32 {
    3
}

fn default_stream_profile() -> StreamProfile {
    StreamProfile::Full
}

fn default_processing_lag_days() -> u32 {
    3
}

fn default_window_in_days() -> u32 {
    1
}

fn default_request_interval_ms() -> u64 {
    300
}

fn default_max_api_retries() -> u32 {
    12
}

impl DataSourceBingWebmasterToolsPluginConfig {
    pub fn validate(&self) -> Result<(), std::io::Error> {
        if self.window_in_days < 1 || self.window_in_days > 364 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "window_in_days must be between 1 and 364",
            ));
        }
        let start = NaiveDate::parse_from_str(&self.start_date, "%Y-%m-%d").map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid start_date: {e}"),
            )
        })?;
        let earliest = Utc::now().date_naive() - Duration::days(BING_HISTORY_MONTHS * 30);
        if start < earliest {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "start_date {start} is older than ~{BING_HISTORY_MONTHS} months of Bing Webmaster history"
                ),
            ));
        }
        Ok(())
    }

    pub fn normalized_site_url(&self) -> String {
        normalize_site_url(&self.site_url)
    }
}

pub struct DataSourceBingWebmasterToolsPlugin {
    config: DataSourceBingWebmasterToolsPluginConfig,
    http: RetryableHttpClient,
    oauth: Option<OAuth2RefreshTokenAuth>,
}

impl DataSourceBingWebmasterToolsPlugin {
    pub fn new(config: DataSourceBingWebmasterToolsPluginConfig) -> Result<Self, std::io::Error> {
        config.validate()?;
        let oauth = Self::build_oauth(&config);
        let http = RetryableHttpClient::new(RetryConfig {
            max_attempts: config.max_api_retries,
            ..RetryConfig::default()
        });
        Ok(Self {
            config,
            http,
            oauth,
        })
    }

    fn build_oauth(
        config: &DataSourceBingWebmasterToolsPluginConfig,
    ) -> Option<OAuth2RefreshTokenAuth> {
        let client_id = config.oauth_client_id.as_deref()?;
        let client_secret = config.oauth_client_secret.as_deref()?;
        let refresh_token = config.oauth_refresh_token.as_deref()?;
        let token_url = config
            .oauth_token_url
            .as_deref()
            .unwrap_or(DEFAULT_BING_OAUTH_TOKEN_URL);
        Some(OAuth2RefreshTokenAuth::new(
            token_url,
            client_id,
            client_secret,
            refresh_token,
        ))
    }

    fn selected_streams(&self) -> Vec<&'static BingStreamDef> {
        resolve_streams(self.config.stream_profile, self.config.streams.clone())
    }

    fn streams_for_run(&self, discover: bool) -> Vec<&'static BingStreamDef> {
        if discover {
            resolve_streams(StreamProfile::Minimal, None)
        } else {
            self.selected_streams()
        }
    }

    fn namespace_contract(stream: &BingStreamDef) -> SourceNamespaceContract {
        let mut primary_key = vec![FieldPath::single("site_url"), FieldPath::single("date")];
        for dim in stream.dimension_fields {
            primary_key.push(FieldPath::single(*dim));
        }
        let description = match stream.kind {
            BingStreamKind::DatedReport => "Bing Webmaster Tools mutable daily report".into(),
            BingStreamKind::PageSnapshot => {
                "Bing Webmaster Tools page traffic snapshot by sync date".into()
            }
            BingStreamKind::SiteRunAggregate => {
                "Bing Webmaster Tools per-run sync aggregate".into()
            }
        };
        SourceNamespaceContract {
            namespace: stream.namespace.to_string(),
            primary_key,
            cursor: Some(FieldPath::single("date")),
            partition_key: vec![FieldPath::single("date")],
            write_policy: WritePolicy::ReplacePartition,
            refresh_window: None,
            description,
            semantics: Some(SourceSemantics::MutableReport),
        }
    }

    fn parse_date(value: &str) -> Result<NaiveDate, std::io::Error> {
        NaiveDate::parse_from_str(value, "%Y-%m-%d").map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid date '{value}': {e}"),
            )
        })
    }

    fn effective_end_date(&self) -> Result<NaiveDate, std::io::Error> {
        let configured = self
            .config
            .end_date
            .as_deref()
            .map(Self::parse_date)
            .transpose()?
            .unwrap_or_else(|| Utc::now().date_naive() - Duration::days(1));
        let lag_boundary =
            Utc::now().date_naive() - Duration::days(self.config.processing_lag_days as i64);
        Ok(configured.min(lag_boundary))
    }

    fn checkpoint_key(&self, namespace: &str) -> String {
        format!(
            "bing_webmaster_tools:{}:{}",
            self.config.normalized_site_url(),
            namespace
        )
    }

    fn load_last_completed(ctx: &dyn SourceSyncContext, key: &str) -> Option<NaiveDate> {
        let Some(cp) = load_checkpoint_payload::<BingNamespaceCheckpoint>(ctx, key) else {
            return None;
        };
        match Self::parse_date(&cp.last_completed_date) {
            Ok(date) => Some(date),
            Err(err) => {
                tracing::warn!("Bing Webmaster ignoring corrupt checkpoint for {key}: {err}");
                None
            }
        }
    }

    fn store_last_completed(
        ctx: &dyn SourceSyncContext,
        key: &str,
        date: NaiveDate,
    ) -> Result<(), std::io::Error> {
        let envelope = CheckpointEnvelope::from_payload(
            CheckpointAuthority::AdvisoryHint,
            CheckpointKind::SourceResume,
            CHECKPOINT_PAYLOAD_VERSION,
            &BingNamespaceCheckpoint {
                last_completed_date: date.format("%Y-%m-%d").to_string(),
            },
        )
        .map_err(|e| std::io::Error::other(e.to_string()))?;
        ctx.store_checkpoint(key, &envelope)
            .map_err(std::io::Error::other)
    }

    async fn auth_mode(&self) -> Result<AuthMode, std::io::Error> {
        authorization_header(
            self.config.api_key.as_deref(),
            self.config.access_token.as_deref(),
            self.oauth.as_ref(),
        )
        .await
    }

    fn submit_rows_for_date(
        &self,
        ctx: &dyn SourceSyncContext,
        namespace: &str,
        date: NaiveDate,
        rows: Vec<serde_json::Value>,
        method: BingApiMethod,
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
        let offset_key = OffsetKey::new(namespace, date.format("%Y-%m-%d").to_string());
        submit_payload_batches(
            ctx,
            vec![IngestBatch {
                offset_key,
                data: payload,
                bytes,
                offset_pos: None,
                source_uri: format!(
                    "bing-webmaster://{}/{}",
                    self.config.normalized_site_url(),
                    crate::bing_api::api_method_name(method)
                ),
                namespace: Some(namespace.to_string()),
                cdc_rows: None,
            }],
        )?;
        Ok(())
    }

    fn group_rows_by_date(rows: Vec<serde_json::Value>) -> HashMap<String, Vec<serde_json::Value>> {
        let mut grouped: HashMap<String, Vec<serde_json::Value>> = HashMap::new();
        for row in rows {
            let date_key = row
                .get("date")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            grouped.entry(date_key).or_default().push(row);
        }
        grouped
    }

    async fn sync_dated_stream(
        &self,
        ctx: Arc<dyn SourceSyncContext>,
        stream: &BingStreamDef,
        auth: &AuthMode,
        discover: bool,
        start_date: NaiveDate,
        end_date: NaiveDate,
    ) -> Result<RunStats, std::io::Error> {
        let site_url = self.config.normalized_site_url();
        let api_rows = fetch_api_rows(&self.http, auth, &site_url, stream.method).await?;
        let checkpoint_key = self.checkpoint_key(stream.namespace);
        let last_completed = if discover {
            None
        } else {
            Self::load_last_completed(ctx.as_ref(), &checkpoint_key)
        };
        let planner = DateWindowPlanner {
            lookback_days: if discover {
                0
            } else {
                self.config.lookback_days
            },
        };
        let window = planner.plan(start_date, last_completed, end_date);
        let dates = DateWindowPlanner::dates_inclusive(&window);
        let bronze = bronze_rows_for_stream(
            &api_rows,
            &site_url,
            stream.dimension_fields,
            None,
            window.start,
            window.end,
        );
        let grouped = Self::group_rows_by_date(bronze);
        let mut stats = RunStats::default();
        stats.rows_synced = grouped.values().map(|v| v.len() as u64).sum();

        for date in dates {
            let date_key = date.format("%Y-%m-%d").to_string();
            let rows_for_day = grouped.get(&date_key).cloned().unwrap_or_default();
            if !discover {
                Self::store_last_completed(ctx.as_ref(), &checkpoint_key, date)?;
            }
            self.submit_rows_for_date(
                ctx.as_ref(),
                stream.namespace,
                date,
                rows_for_day,
                stream.method,
            )?;
        }

        if self.config.request_interval_ms > 0 {
            sleep(TokioDuration::from_millis(self.config.request_interval_ms)).await;
        }
        Ok(stats)
    }

    async fn sync_page_snapshot(
        &self,
        ctx: Arc<dyn SourceSyncContext>,
        stream: &BingStreamDef,
        auth: &AuthMode,
        partition_date: NaiveDate,
    ) -> Result<u64, std::io::Error> {
        let site_url = self.config.normalized_site_url();
        let api_rows = fetch_api_rows(&self.http, auth, &site_url, stream.method).await?;
        let bronze = bronze_rows_for_stream(
            &api_rows,
            &site_url,
            stream.dimension_fields,
            Some(partition_date),
            partition_date,
            partition_date,
        );
        let count = bronze.len() as u64;
        self.submit_rows_for_date(
            ctx.as_ref(),
            stream.namespace,
            partition_date,
            bronze,
            stream.method,
        )?;
        Ok(count)
    }

    async fn emit_site_run_daily(
        &self,
        ctx: Arc<dyn SourceSyncContext>,
        partition_date: NaiveDate,
        stats: &RunStats,
    ) -> Result<(), std::io::Error> {
        let row = serde_json::json!({
            "site_url": self.config.normalized_site_url(),
            "date": partition_date.format("%Y-%m-%d").to_string(),
            "rows_synced": stats.rows_synced,
            "api_errors": stats.api_errors,
        });
        self.submit_rows_for_date(
            ctx.as_ref(),
            "bing_webmaster_tools.site_run_daily",
            partition_date,
            vec![row],
            BingApiMethod::GetRankAndTrafficStats,
        )
    }
}

#[derive(Default)]
struct RunStats {
    rows_synced: u64,
    api_errors: u64,
}

#[async_trait]
impl DataSource for DataSourceBingWebmasterToolsPlugin {
    fn execution_contract(&self) -> SourceExecutionContract {
        SourceExecutionContract::stream(SourceOnceContract::Finite)
    }

    fn source_namespace_contracts(&self) -> Vec<SourceNamespaceContract> {
        let contracts: Vec<_> = self
            .selected_streams()
            .iter()
            .map(|stream| {
                let mut contract = Self::namespace_contract(stream);
                contract.refresh_window = Some(self.config.lookback_days);
                contract
            })
            .collect();
        for contract in &contracts {
            contract
                .validate()
                .expect("invalid Bing Webmaster namespace contract configuration");
        }
        contracts
    }

    async fn sync(&mut self, ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error> {
        self.config.validate()?;

        for contract in self.source_namespace_contracts() {
            contract
                .validate()
                .map_err(|err| std::io::Error::other(err.to_string()))?;
        }

        let discover = runtime_is_discover_mode();
        let end_date = self.effective_end_date()?;
        let configured_start = Self::parse_date(&self.config.start_date)?;
        let start_date = if discover {
            let (sample_start, _) = discover_sample_date_window(end_date);
            info!(
                discover_sample_days = DISCOVER_SAMPLE_DAYS,
                sample_start = %sample_start,
                sample_end = %end_date,
                configured_start = %configured_start,
                "Bing Webmaster discover: sampling recent days only; full historical sync runs on skippr sync"
            );
            sample_start
        } else {
            configured_start
        };
        if end_date < start_date {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Bing Webmaster end date {end_date} is before start date {start_date}"),
            ));
        }

        let auth = self.auth_mode().await?;
        let streams = self.streams_for_run(discover);
        if discover {
            info!(
                stream_count = streams.len(),
                stream_profile = "minimal",
                "Bing Webmaster discover: sampling minimal stream profile for API calls"
            );
        }

        let mut run_stats = RunStats::default();
        let run_date = end_date;

        for stream in streams {
            if stream.kind == BingStreamKind::SiteRunAggregate {
                continue;
            }
            match stream.kind {
                BingStreamKind::DatedReport => {
                    match self
                        .sync_dated_stream(
                            Arc::clone(&ctx),
                            stream,
                            &auth,
                            discover,
                            start_date,
                            end_date,
                        )
                        .await
                    {
                        Ok(stats) => {
                            run_stats.rows_synced += stats.rows_synced;
                            run_stats.api_errors += stats.api_errors;
                        }
                        Err(err) if is_forbidden_site_error(&err) => {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::PermissionDenied,
                                format!(
                                    "Bing Webmaster permission denied for site_url '{}': {err}. \
                                     Verify the API key or OAuth account has access to the site.",
                                    self.config.normalized_site_url()
                                ),
                            ));
                        }
                        Err(err) => return Err(err),
                    }
                }
                BingStreamKind::PageSnapshot => {
                    if !discover {
                        let count = self
                            .sync_page_snapshot(Arc::clone(&ctx), stream, &auth, run_date)
                            .await?;
                        run_stats.rows_synced += count;
                    }
                }
                BingStreamKind::SiteRunAggregate => {}
            }
        }

        if !discover
            && self
                .selected_streams()
                .iter()
                .any(|s| s.kind == BingStreamKind::SiteRunAggregate)
        {
            self.emit_site_run_daily(ctx, run_date, &run_stats).await?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::streams::{StreamProfile, FULL_STREAM_COUNT};
    use crate::test_env;

    fn test_config() -> DataSourceBingWebmasterToolsPluginConfig {
        DataSourceBingWebmasterToolsPluginConfig {
            site_url: "https://example.com/".into(),
            api_key: Some("test-key".into()),
            access_token: None,
            oauth_token_url: None,
            oauth_client_id: None,
            oauth_client_secret: None,
            oauth_refresh_token: None,
            start_date: "2026-03-01".into(),
            end_date: None,
            lookback_days: 3,
            stream_profile: StreamProfile::Full,
            processing_lag_days: 3,
            window_in_days: 1,
            streams: None,
            request_interval_ms: 0,
            max_api_retries: 12,
        }
    }

    #[test]
    fn config_defaults() {
        let cfg: DataSourceBingWebmasterToolsPluginConfig =
            serde_json::from_value(serde_json::json!({
                "site_url": "https://example.com/",
                "api_key": "key",
                "start_date": "2026-03-01"
            }))
            .expect("deserialize");
        assert_eq!(cfg.stream_profile, StreamProfile::Full);
        assert_eq!(cfg.lookback_days, 3);
        assert_eq!(cfg.processing_lag_days, 3);
    }

    #[test]
    fn discover_sample_window_is_last_three_days_inclusive() {
        let end = NaiveDate::from_ymd_opt(2026, 5, 25).unwrap();
        let (start, end_out) = discover_sample_date_window(end);
        assert_eq!(end_out, end);
        assert_eq!(start, NaiveDate::from_ymd_opt(2026, 5, 23).unwrap());
    }

    #[test]
    fn discover_mode_uses_minimal_streams_only() {
        let plugin = DataSourceBingWebmasterToolsPlugin::new(test_config()).unwrap();
        let discover_streams = plugin.streams_for_run(true);
        let sync_streams = plugin.streams_for_run(false);
        assert_eq!(discover_streams.len(), 1);
        assert_eq!(sync_streams.len(), FULL_STREAM_COUNT);
    }

    #[test]
    fn contracts_validate_for_full_profile() {
        let plugin = DataSourceBingWebmasterToolsPlugin::new(test_config()).unwrap();
        let contracts = plugin.source_namespace_contracts();
        assert_eq!(contracts.len(), FULL_STREAM_COUNT);
        for contract in contracts {
            contract.validate().expect("valid contract");
            assert_eq!(contract.write_policy, WritePolicy::ReplacePartition);
            assert_eq!(contract.partition_key.len(), 1);
        }
    }

    #[test]
    fn loads_fixture_dir_for_rank_and_traffic_stats() {
        let _guard = test_env::lock();
        test_env::clear_discover_mode();
        test_env::set_fixture_dir();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let rows = rt
            .block_on(async {
                let http = RetryableHttpClient::new(RetryConfig::default());
                fetch_api_rows(
                    &http,
                    &AuthMode::Fixture,
                    "https://example.com/",
                    BingApiMethod::GetRankAndTrafficStats,
                )
                .await
            })
            .expect("fixture rows");
        assert!(!rows.is_empty());
        test_env::clear_fixture_dir();
    }

    #[test]
    fn missing_credentials_returns_clear_error() {
        let _guard = test_env::lock();
        test_env::clear_fixture_dir();
        let cfg = DataSourceBingWebmasterToolsPluginConfig {
            site_url: "https://example.com/".into(),
            api_key: None,
            access_token: None,
            oauth_token_url: None,
            oauth_client_id: None,
            oauth_client_secret: None,
            oauth_refresh_token: None,
            start_date: "2026-03-01".into(),
            end_date: None,
            lookback_days: 3,
            stream_profile: StreamProfile::Minimal,
            processing_lag_days: 3,
            window_in_days: 1,
            streams: None,
            request_interval_ms: 0,
            max_api_retries: 3,
        };
        let plugin = DataSourceBingWebmasterToolsPlugin::new(cfg).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt
            .block_on(plugin.auth_mode())
            .expect_err("auth should fail");
        assert!(
            err.to_string().contains("api_key")
                || err.to_string().contains("access_token")
                || err.to_string().contains("OAuth")
        );
    }

    #[test]
    fn discover_sync_does_not_store_checkpoints() {
        let _guard = test_env::lock();
        test_env::clear_discover_mode();
        test_env::set_fixture_dir();
        std::env::set_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV, "discover");

        let mut cfg = test_config();
        cfg.end_date = Some("2026-03-01".into());
        cfg.stream_profile = StreamProfile::Minimal;
        cfg.processing_lag_days = 0;
        cfg.api_key = None;
        let mut plugin = DataSourceBingWebmasterToolsPlugin::new(cfg).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        rt.block_on(plugin.sync(ctx.clone()))
            .expect("discover sync");
        assert!(ctx.checkpoint_stores.lock().unwrap().is_empty());

        test_env::clear_fixture_dir();
        test_env::clear_discover_mode();
    }

    #[test]
    fn invalid_start_date_rejected_at_new() {
        let cfg = DataSourceBingWebmasterToolsPluginConfig {
            site_url: "https://example.com/".into(),
            api_key: Some("key".into()),
            start_date: "not-a-date".into(),
            ..test_config()
        };
        match DataSourceBingWebmasterToolsPlugin::new(cfg) {
            Err(err) => {
                assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
                assert!(err.to_string().contains("start_date"));
            }
            Ok(_) => panic!("expected invalid start_date"),
        }
    }

    #[test]
    fn start_date_older_than_history_rejected() {
        let cfg = DataSourceBingWebmasterToolsPluginConfig {
            site_url: "https://example.com/".into(),
            api_key: Some("key".into()),
            start_date: "2010-01-01".into(),
            ..test_config()
        };
        match DataSourceBingWebmasterToolsPlugin::new(cfg) {
            Err(err) => assert!(err.to_string().contains("months")),
            Ok(_) => panic!("expected start_date too old"),
        }
    }

    #[test]
    fn invalid_window_in_days_rejected() {
        let cfg = DataSourceBingWebmasterToolsPluginConfig {
            window_in_days: 0,
            ..test_config()
        };
        match DataSourceBingWebmasterToolsPlugin::new(cfg) {
            Err(err) => assert!(err.to_string().contains("window_in_days")),
            Ok(_) => panic!("expected invalid window_in_days"),
        }
    }

    #[test]
    fn sync_errors_when_end_before_start() {
        let _guard = test_env::lock();
        test_env::clear_discover_mode();
        test_env::clear_fixture_dir();

        let cfg = DataSourceBingWebmasterToolsPluginConfig {
            start_date: "2026-03-10".into(),
            end_date: Some("2026-03-01".into()),
            stream_profile: StreamProfile::Minimal,
            processing_lag_days: 0,
            ..test_config()
        };
        let mut plugin = DataSourceBingWebmasterToolsPlugin::new(cfg).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        let err = rt.block_on(plugin.sync(ctx)).expect_err("end before start");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("before start"));
    }

    #[test]
    fn checkpoint_roundtrip() {
        let cp = BingNamespaceCheckpoint {
            last_completed_date: "2026-03-01".into(),
        };
        let envelope = CheckpointEnvelope::from_payload(
            CheckpointAuthority::AdvisoryHint,
            CheckpointKind::SourceResume,
            CHECKPOINT_PAYLOAD_VERSION,
            &cp,
        )
        .expect("envelope");
        let decoded: BingNamespaceCheckpoint = envelope.into_payload().expect("payload");
        assert_eq!(decoded.last_completed_date, "2026-03-01");
    }

    #[test]
    fn query_daily_contract_includes_query_dimension() {
        let plugin = DataSourceBingWebmasterToolsPlugin::new(test_config()).unwrap();
        let contract = plugin
            .source_namespace_contracts()
            .into_iter()
            .find(|c| c.namespace == "bing_webmaster_tools.query_daily")
            .expect("query_daily contract");
        assert!(contract.primary_key.iter().any(|p| p.0 == ["Query"]));
    }

    #[test]
    fn contracts_set_refresh_window_from_lookback() {
        let plugin = DataSourceBingWebmasterToolsPlugin::new(test_config()).unwrap();
        assert!(plugin
            .source_namespace_contracts()
            .iter()
            .all(|c| c.refresh_window == Some(3)));
    }

    #[test]
    fn execution_contract_is_finite_once() {
        let plugin = DataSourceBingWebmasterToolsPlugin::new(test_config()).unwrap();
        assert_eq!(plugin.execution_contract().once, SourceOnceContract::Finite);
    }

    #[test]
    fn full_sync_stores_checkpoints_for_dated_stream() {
        let _guard = test_env::lock();
        test_env::clear_discover_mode();
        test_env::set_fixture_dir();

        let cfg = DataSourceBingWebmasterToolsPluginConfig {
            start_date: "2026-03-01".into(),
            end_date: Some("2026-03-01".into()),
            stream_profile: StreamProfile::Minimal,
            processing_lag_days: 0,
            lookback_days: 0,
            api_key: None,
            ..test_config()
        };
        let mut plugin = DataSourceBingWebmasterToolsPlugin::new(cfg).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        rt.block_on(plugin.sync(ctx.clone())).expect("sync");
        let keys = ctx.checkpoint_stores.lock().unwrap();
        assert!(keys.iter().any(|k| k.contains("site_daily")));

        test_env::clear_fixture_dir();
    }

    #[test]
    fn full_profile_sync_emits_site_run_daily() {
        let _guard = test_env::lock();
        test_env::clear_discover_mode();
        test_env::set_fixture_dir();

        let cfg = DataSourceBingWebmasterToolsPluginConfig {
            start_date: "2026-03-01".into(),
            end_date: Some("2026-03-01".into()),
            stream_profile: StreamProfile::Full,
            processing_lag_days: 0,
            lookback_days: 0,
            request_interval_ms: 0,
            api_key: None,
            ..test_config()
        };
        let mut plugin = DataSourceBingWebmasterToolsPlugin::new(cfg).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        rt.block_on(plugin.sync(ctx.clone())).expect("sync");
        let namespaces = ctx.submitted_namespaces();
        assert!(namespaces.contains(&"bing_webmaster_tools.site_run_daily".to_string()));

        test_env::clear_fixture_dir();
    }

    #[test]
    fn standard_sync_includes_query_and_crawl_not_site_run() {
        let _guard = test_env::lock();
        test_env::clear_discover_mode();
        test_env::set_fixture_dir();

        let cfg = DataSourceBingWebmasterToolsPluginConfig {
            start_date: "2026-03-01".into(),
            end_date: Some("2026-03-01".into()),
            stream_profile: StreamProfile::Standard,
            processing_lag_days: 0,
            lookback_days: 0,
            request_interval_ms: 0,
            api_key: None,
            ..test_config()
        };
        let mut plugin = DataSourceBingWebmasterToolsPlugin::new(cfg).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        rt.block_on(plugin.sync(ctx.clone())).expect("sync");
        let namespaces = ctx.submitted_namespaces();
        assert!(namespaces.contains(&"bing_webmaster_tools.site_daily".to_string()));
        assert!(namespaces.contains(&"bing_webmaster_tools.query_daily".to_string()));
        assert!(namespaces.contains(&"bing_webmaster_tools.crawl_daily".to_string()));
        assert!(!namespaces.contains(&"bing_webmaster_tools.page_daily".to_string()));
        assert!(!namespaces.contains(&"bing_webmaster_tools.site_run_daily".to_string()));

        test_env::clear_fixture_dir();
    }

    #[test]
    fn discover_sync_submits_only_site_daily_namespace() {
        let _guard = test_env::lock();
        test_env::clear_discover_mode();
        test_env::set_fixture_dir();
        std::env::set_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV, "discover");

        let mut cfg = test_config();
        cfg.end_date = Some("2026-03-02".into());
        cfg.stream_profile = StreamProfile::Full;
        cfg.processing_lag_days = 0;
        cfg.api_key = None;
        let mut plugin = DataSourceBingWebmasterToolsPlugin::new(cfg).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        rt.block_on(plugin.sync(ctx.clone()))
            .expect("discover sync");
        let namespaces = ctx.submitted_namespaces();
        assert!(!namespaces.is_empty());
        assert!(namespaces
            .iter()
            .all(|n| n == "bing_webmaster_tools.site_daily"));
        assert!(!namespaces
            .iter()
            .any(|n| n.contains("query_daily") || n.contains("page_daily")));

        test_env::clear_fixture_dir();
        test_env::clear_discover_mode();
    }

    #[test]
    fn is_forbidden_site_error_maps_to_permission_denied() {
        assert!(is_forbidden_site_error(&std::io::Error::other(
            "Bing Webmaster request failed: HTTP 403 Forbidden"
        )));
    }

    #[test]
    fn loads_all_fixture_endpoints() {
        let _guard = test_env::lock();
        test_env::clear_discover_mode();
        test_env::set_fixture_dir();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let http = RetryableHttpClient::new(RetryConfig::default());
        let auth = AuthMode::Fixture;
        let site = "https://example.com/";
        rt.block_on(async {
            for method in [
                BingApiMethod::GetRankAndTrafficStats,
                BingApiMethod::GetQueryStats,
                BingApiMethod::GetPageStats,
                BingApiMethod::GetCrawlStats,
            ] {
                let rows = fetch_api_rows(&http, &auth, site, method)
                    .await
                    .unwrap_or_else(|e| panic!("{method:?}: {e}"));
                assert!(!rows.is_empty(), "{method:?} fixture empty");
            }
        });
        test_env::clear_fixture_dir();
    }

    use skippr_runtime_sdk::plugins::{
        OffsetValidationEntry, SourcePayloadTask, SourceSyncContext,
    };
    use skippr_runtime_sdk::protocol::RuntimeOffsetMaterializationHint;
    use skippr_runtime_sdk::source_compat::ThroughputMetrics;

    #[derive(Default)]
    struct RecordingSyncContext {
        checkpoint_stores: Mutex<Vec<String>>,
        payload_tasks: Mutex<Vec<SourcePayloadTask>>,
    }

    impl RecordingSyncContext {
        fn submitted_namespaces(&self) -> Vec<String> {
            self.payload_tasks
                .lock()
                .unwrap()
                .iter()
                .flat_map(|task| {
                    task.batches
                        .iter()
                        .filter_map(|batch| batch.namespace.clone())
                })
                .collect()
        }
    }

    impl SourceSyncContext for RecordingSyncContext {
        fn submit_payload_tasks(
            &self,
            tasks: Vec<SourcePayloadTask>,
        ) -> Result<ThroughputMetrics, std::io::Error> {
            self.payload_tasks.lock().unwrap().extend(tasks);
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
