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
use tracing::{info, warn};

use crate::gsc_api::{
    authorization_header, is_forbidden_site_error, is_optional_stream_error, normalize_site_url,
    parse_query_response_rows, search_analytics_query_all, validate_site_access,
};
use crate::sitemaps::sitemap_rows_for_date;
use crate::streams::{
    resolve_streams, GscStreamDef, GscStreamKind, StreamProfile, URL_INSPECTION_NAMESPACE,
};
use crate::url_inspection::inspection_rows_for_date;

const CHECKPOINT_PAYLOAD_VERSION: u32 = 1;
const DISCOVER_SAMPLE_DAYS: u32 = 3;
const GAADS_HISTORY_MONTHS: i64 = 16;

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
struct GscNamespaceCheckpoint {
    last_completed_date: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DataSourceGoogleAdsPluginConfig {
    pub site_url: String,
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
    #[serde(default)]
    pub service_account_json_path: Option<String>,
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
    #[serde(default = "default_search_type")]
    pub search_type: String,
    #[serde(default = "default_data_state")]
    pub data_state: String,
    #[serde(default = "default_row_limit")]
    pub row_limit: u32,
    #[serde(default = "default_request_interval_ms")]
    pub request_interval_ms: u64,
    #[serde(default = "default_max_api_retries")]
    pub max_api_retries: u32,
    #[serde(default)]
    pub url_inspection_enabled: bool,
    #[serde(default)]
    pub url_list: Vec<String>,
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

fn default_search_type() -> String {
    "web".into()
}

fn default_data_state() -> String {
    "final".into()
}

fn default_row_limit() -> u32 {
    25_000
}

fn default_request_interval_ms() -> u64 {
    300
}

fn default_max_api_retries() -> u32 {
    12
}

impl DataSourceGoogleAdsPluginConfig {
    pub fn validate(&self) -> Result<(), std::io::Error> {
        if self.window_in_days < 1 || self.window_in_days > 364 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "window_in_days must be between 1 and 364",
            ));
        }
        if self.row_limit < 1 || self.row_limit > 25_000 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "row_limit must be between 1 and 25000",
            ));
        }
        let start = NaiveDate::parse_from_str(&self.start_date, "%Y-%m-%d").map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid start_date: {e}"),
            )
        })?;
        let earliest = Utc::now().date_naive() - Duration::days(GAADS_HISTORY_MONTHS * 30);
        if start < earliest {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "start_date {start} is older than ~{GAADS_HISTORY_MONTHS} months of Search Console history"
                ),
            ));
        }
        Ok(())
    }

    pub fn normalized_site_url(&self) -> String {
        normalize_site_url(&self.site_url)
    }
}

pub struct DataSourceGoogleAdsPlugin {
    config: DataSourceGoogleAdsPluginConfig,
    http: RetryableHttpClient,
    oauth: Option<OAuth2RefreshTokenAuth>,
}

impl DataSourceGoogleAdsPlugin {
    pub fn new(config: DataSourceGoogleAdsPluginConfig) -> Result<Self, std::io::Error> {
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
        config: &DataSourceGoogleAdsPluginConfig,
    ) -> Option<OAuth2RefreshTokenAuth> {
        let token_url = config.oauth_token_url.as_deref()?;
        let client_id = config.oauth_client_id.as_deref()?;
        let client_secret = config.oauth_client_secret.as_deref()?;
        let refresh_token = config.oauth_refresh_token.as_deref()?;
        Some(OAuth2RefreshTokenAuth::new(
            token_url,
            client_id,
            client_secret,
            refresh_token,
        ))
    }

    fn selected_streams(&self) -> Vec<&'static GscStreamDef> {
        resolve_streams(self.config.stream_profile, self.config.streams.clone())
    }

    fn streams_for_run(&self, discover: bool) -> Vec<&'static GscStreamDef> {
        if discover {
            resolve_streams(StreamProfile::Minimal, None)
        } else {
            self.selected_streams()
        }
    }

    fn namespace_contract(stream: &GscStreamDef) -> SourceNamespaceContract {
        let mut primary_key = vec![FieldPath::single("site_url"), FieldPath::single("date")];
        for dim in stream.dimensions {
            if *dim != "date" {
                primary_key.push(FieldPath::single(*dim));
            }
        }
        let description = match stream.kind {
            GscStreamKind::SearchAnalytics => {
                "Google Search Console Search Analytics mutable daily report".into()
            }
            GscStreamKind::SitemapSnapshot => {
                "Google Search Console sitemap snapshot by sync date".into()
            }
            GscStreamKind::SiteRunAggregate => {
                "Google Search Console per-run sync aggregate".into()
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

    fn url_inspection_contract() -> SourceNamespaceContract {
        SourceNamespaceContract {
            namespace: URL_INSPECTION_NAMESPACE.to_string(),
            primary_key: vec![
                FieldPath::single("site_url"),
                FieldPath::single("date"),
                FieldPath::single("inspection_url"),
            ],
            cursor: Some(FieldPath::single("date")),
            partition_key: vec![FieldPath::single("date")],
            write_policy: WritePolicy::ReplacePartition,
            refresh_window: None,
            description: "Google Search Console URL Inspection API results".into(),
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
            "gaads:{}:{}",
            self.config.normalized_site_url(),
            namespace
        )
    }

    fn load_last_completed(ctx: &dyn SourceSyncContext, key: &str) -> Option<NaiveDate> {
        let Some(cp) = load_checkpoint_payload::<GscNamespaceCheckpoint>(ctx, key) else {
            return None;
        };
        match Self::parse_date(&cp.last_completed_date) {
            Ok(date) => Some(date),
            Err(err) => {
                tracing::warn!("GAADS ignoring corrupt checkpoint for {key}: {err}");
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
            &GscNamespaceCheckpoint {
                last_completed_date: date.format("%Y-%m-%d").to_string(),
            },
        )
        .map_err(|e| std::io::Error::other(e.to_string()))?;
        ctx.store_checkpoint(key, &envelope)
            .map_err(std::io::Error::other)
    }

    async fn auth_header(&self) -> Result<String, std::io::Error> {
        if std::env::var("SKIPPR_GOOGLE_ADS_FIXTURE_DIR")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .is_some()
        {
            return Ok("Bearer fixture".into());
        }
        authorization_header(
            self.config.access_token.as_deref(),
            self.oauth.as_ref(),
            self.config.service_account_json_path.as_deref(),
        )
        .await
    }

    fn submit_rows_for_date(
        &self,
        ctx: &dyn SourceSyncContext,
        namespace: &str,
        date: NaiveDate,
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
        let offset_key = OffsetKey::new(namespace, date.format("%Y-%m-%d").to_string());
        submit_payload_batches(
            ctx,
            vec![IngestBatch {
                offset_key,
                data: payload,
                bytes,
                source_uri: format!(
                    "gaads://{}/searchAnalytics",
                    self.config.normalized_site_url()
                ),
                namespace: Some(namespace.to_string()),
                cdc_rows: None,
            }],
        )?;
        Ok(())
    }

    async fn sync_search_analytics_stream(
        &self,
        ctx: Arc<dyn SourceSyncContext>,
        stream: &GscStreamDef,
        auth_header: &str,
        discover: bool,
        start_date: NaiveDate,
        end_date: NaiveDate,
    ) -> Result<RunStats, std::io::Error> {
        let mut stats = RunStats::default();
        let checkpoint_key = self.checkpoint_key(stream.namespace);
        let last_completed = if discover {
            None
        } else {
            Self::load_last_completed(ctx.as_ref(), &checkpoint_key)
        };
        let planner = DateWindowPlanner {
            lookback_days: if discover { 0 } else { self.config.lookback_days },
        };
        let window = planner.plan(start_date, last_completed, end_date);
        let dates = DateWindowPlanner::dates_inclusive(&window);
        let chunks = chunk_dates(&dates, self.config.window_in_days);
        let site_url = self.config.normalized_site_url();

        for (chunk_start, chunk_end) in chunks {
            match search_analytics_query_all(
                &self.http,
                auth_header,
                &site_url,
                chunk_start,
                chunk_end,
                stream.dimensions,
                &self.config.search_type,
                &self.config.data_state,
                self.config.row_limit,
            )
            .await
            {
                Ok(api_rows) => {
                    stats.rows_synced += api_rows.len() as u64;
                    let rows = parse_query_response_rows(
                        &api_rows,
                        stream.dimensions,
                        &site_url,
                        &self.config.search_type,
                    );
                    let grouped = group_rows_by_date(rows);
                    let mut cursor = chunk_start;
                    while cursor <= chunk_end {
                        if !discover {
                            Self::store_last_completed(ctx.as_ref(), &checkpoint_key, cursor)?;
                        }
                        let date_key = cursor.format("%Y-%m-%d").to_string();
                        let rows_for_day = grouped.get(&date_key).cloned().unwrap_or_default();
                        self.submit_rows_for_date(
                            ctx.as_ref(),
                            stream.namespace,
                            cursor,
                            rows_for_day,
                        )?;
                        cursor += Duration::days(1);
                    }
                    if self.config.request_interval_ms > 0 {
                        sleep(TokioDuration::from_millis(self.config.request_interval_ms)).await;
                    }
                }
                Err(err) if stream.optional && is_optional_stream_error(&err) => {
                    warn!(
                        "GAADS skipping optional stream {}: {err}",
                        stream.namespace
                    );
                    stats.api_errors += 1;
                    break;
                }
                Err(err) if is_forbidden_site_error(&err) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        format!(
                            "GAADS permission denied for site_url '{}': {err}. Verify the OAuth \
                             account or service account is added as a user on the property in \
                             Search Console.",
                            site_url
                        ),
                    ));
                }
                Err(err) => return Err(err),
            }
        }
        Ok(stats)
    }

    async fn sync_sitemap_snapshot(
        &self,
        ctx: Arc<dyn SourceSyncContext>,
        auth_header: &str,
        partition_date: NaiveDate,
    ) -> Result<u64, std::io::Error> {
        let rows = sitemap_rows_for_date(
            &self.http,
            auth_header,
            &self.config.normalized_site_url(),
            partition_date,
        )
        .await?;
        let count = rows.len() as u64;
        self.submit_rows_for_date(
            ctx.as_ref(),
            "google_ads.sitemap_daily",
            partition_date,
            rows,
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
            "search_type": self.config.search_type,
            "rows_synced": stats.rows_synced,
            "api_errors": stats.api_errors,
        });
        self.submit_rows_for_date(
            ctx.as_ref(),
            "google_ads.site_run_daily",
            partition_date,
            vec![row],
        )
    }
}

#[derive(Default)]
struct RunStats {
    rows_synced: u64,
    api_errors: u64,
}

fn chunk_dates(dates: &[NaiveDate], window_in_days: u32) -> Vec<(NaiveDate, NaiveDate)> {
    if dates.is_empty() {
        return Vec::new();
    }
    let window = window_in_days.max(1) as usize;
    let mut chunks = Vec::new();
    let mut index = 0;
    while index < dates.len() {
        let start = dates[index];
        let end_index = (index + window - 1).min(dates.len() - 1);
        let end = dates[end_index];
        chunks.push((start, end));
        index = end_index + 1;
    }
    chunks
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

#[async_trait]
impl DataSource for DataSourceGoogleAdsPlugin {
    fn execution_contract(&self) -> SourceExecutionContract {
        SourceExecutionContract::stream(SourceOnceContract::Finite)
    }

    fn source_namespace_contracts(&self) -> Vec<SourceNamespaceContract> {
        let mut contracts: Vec<_> = self
            .selected_streams()
            .iter()
            .map(|stream| {
                let mut contract = Self::namespace_contract(stream);
                contract.refresh_window = Some(self.config.lookback_days);
                contract
            })
            .collect();
        if self.config.url_inspection_enabled && !self.config.url_list.is_empty() {
            contracts.push(Self::url_inspection_contract());
        }
        for contract in &contracts {
            contract
                .validate()
                .expect("invalid GAADS namespace contract configuration");
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
                "GAADS discover: sampling recent days only; full historical sync runs on skippr sync"
            );
            sample_start
        } else {
            configured_start
        };
        if end_date < start_date {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("GAADS end date {end_date} is before start date {start_date}"),
            ));
        }

        let auth_header = self.auth_header().await?;
        if !discover
            && std::env::var("SKIPPR_GOOGLE_ADS_FIXTURE_DIR")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .is_none()
        {
            validate_site_access(&self.http, &auth_header, &self.config.normalized_site_url())
                .await?;
        }

        let streams = self.streams_for_run(discover);
        if discover {
            info!(
                stream_count = streams.len(),
                stream_profile = "minimal",
                "GAADS discover: sampling minimal stream profile for API calls"
            );
        }

        let mut run_stats = RunStats::default();
        let run_date = end_date;

        for stream in streams {
            match stream.kind {
                GscStreamKind::SearchAnalytics => {
                    let stats = self
                        .sync_search_analytics_stream(
                            Arc::clone(&ctx),
                            stream,
                            &auth_header,
                            discover,
                            start_date,
                            end_date,
                        )
                        .await?;
                    run_stats.rows_synced += stats.rows_synced;
                    run_stats.api_errors += stats.api_errors;
                }
                GscStreamKind::SitemapSnapshot => {
                    if !discover {
                        let count = self
                            .sync_sitemap_snapshot(Arc::clone(&ctx), &auth_header, run_date)
                            .await?;
                        run_stats.rows_synced += count;
                    }
                }
                GscStreamKind::SiteRunAggregate => {}
            }
        }

        if !discover
            && self.config.url_inspection_enabled
            && !self.config.url_list.is_empty()
        {
            let rows = inspection_rows_for_date(
                &self.http,
                &auth_header,
                &self.config.normalized_site_url(),
                &self.config.url_list,
                run_date,
            )
            .await?;
            run_stats.rows_synced += rows.len() as u64;
            self.submit_rows_for_date(
                ctx.as_ref(),
                URL_INSPECTION_NAMESPACE,
                run_date,
                rows,
            )?;
        }

        if !discover
            && self
                .selected_streams()
                .iter()
                .any(|s| s.kind == GscStreamKind::SiteRunAggregate)
        {
            self.emit_site_run_daily(ctx, run_date, &run_stats).await?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{LazyLock, Mutex};

    use super::*;
    use crate::streams::{streams_for_profile, StreamProfile, FULL_STREAM_COUNT};
    use skippr_plugin_shared_api_source::DateWindow;

    static ENV_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn test_config() -> DataSourceGoogleAdsPluginConfig {
        DataSourceGoogleAdsPluginConfig {
            site_url: "https://example.com/".into(),
            access_token: Some("token".into()),
            oauth_token_url: None,
            oauth_client_id: None,
            oauth_client_secret: None,
            oauth_refresh_token: None,
            service_account_json_path: None,
            start_date: "2025-06-01".into(),
            end_date: None,
            lookback_days: 3,
            stream_profile: StreamProfile::Full,
            processing_lag_days: 3,
            window_in_days: 1,
            streams: None,
            search_type: "web".into(),
            data_state: "final".into(),
            row_limit: 25_000,
            request_interval_ms: 300,
            max_api_retries: 12,
            url_inspection_enabled: false,
            url_list: vec![],
        }
    }

    #[test]
    fn accuracy_first_config_defaults() {
        let cfg: DataSourceGoogleAdsPluginConfig =
            serde_json::from_value(serde_json::json!({
                "site_url": "https://example.com/",
                "start_date": "2025-06-01"
            }))
            .expect("deserialize");
        assert_eq!(cfg.stream_profile, StreamProfile::Full);
        assert_eq!(cfg.lookback_days, 3);
        assert_eq!(cfg.processing_lag_days, 3);
        assert_eq!(cfg.search_type, "web");
        assert_eq!(cfg.data_state, "final");
    }

    #[test]
    fn discover_sample_window_is_last_three_days_inclusive() {
        let end = NaiveDate::from_ymd_opt(2026, 5, 25).unwrap();
        let (start, end_out) = discover_sample_date_window(end);
        assert_eq!(end_out, end);
        assert_eq!(start, NaiveDate::from_ymd_opt(2026, 5, 23).unwrap());
        assert_eq!(
            DateWindowPlanner::dates_inclusive(&DateWindow { start, end: end_out }).len(),
            DISCOVER_SAMPLE_DAYS as usize
        );
    }

    #[test]
    fn discover_mode_uses_minimal_streams_only() {
        let plugin = DataSourceGoogleAdsPlugin::new(test_config()).unwrap();
        let discover_streams = plugin.streams_for_run(true);
        let sync_streams = plugin.streams_for_run(false);
        assert_eq!(discover_streams.len(), 1);
        assert_eq!(sync_streams.len(), FULL_STREAM_COUNT);
    }

    #[test]
    fn checkpoint_advances_date() {
        let cp = GscNamespaceCheckpoint {
            last_completed_date: "2024-03-01".into(),
        };
        let envelope = CheckpointEnvelope::from_payload(
            CheckpointAuthority::AdvisoryHint,
            CheckpointKind::SourceResume,
            CHECKPOINT_PAYLOAD_VERSION,
            &cp,
        )
        .expect("checkpoint envelope");
        let decoded: GscNamespaceCheckpoint = envelope.into_payload().expect("payload");
        assert_eq!(decoded.last_completed_date, "2024-03-01");
    }

    #[test]
    fn namespace_contracts_use_replace_partition() {
        let plugin = DataSourceGoogleAdsPlugin::new(test_config()).unwrap();
        let contracts = plugin.source_namespace_contracts();
        assert_eq!(contracts.len(), FULL_STREAM_COUNT);
        assert!(contracts
            .iter()
            .all(|c| c.write_policy == WritePolicy::ReplacePartition));
    }

    #[test]
    fn loads_fixture_dir_for_search_analytics() {
        let _lock = env_lock();
        let fixture_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        std::env::set_var("SKIPPR_GOOGLE_ADS_FIXTURE_DIR", fixture_dir);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let rows = rt
            .block_on(async {
                let http = RetryableHttpClient::new(RetryConfig::default());
                search_analytics_query_all(
                    &http,
                    "Bearer fixture",
                    "https://example.com/",
                    NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(),
                    NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(),
                    &["date", "query"],
                    "web",
                    "final",
                    25_000,
                )
                .await
            })
            .expect("fixture rows");
        assert!(!rows.is_empty());
        std::env::remove_var("SKIPPR_GOOGLE_ADS_FIXTURE_DIR");
    }

    #[test]
    fn missing_credentials_returns_clear_error() {
        let _lock = env_lock();
        let saved_gac = std::env::var("GOOGLE_APPLICATION_CREDENTIALS").ok();
        let cfg = DataSourceGoogleAdsPluginConfig {
            site_url: "https://example.com/".into(),
            access_token: None,
            oauth_token_url: None,
            oauth_client_id: None,
            oauth_client_secret: None,
            oauth_refresh_token: None,
            service_account_json_path: None,
            start_date: "2025-06-01".into(),
            end_date: None,
            lookback_days: 3,
            stream_profile: StreamProfile::Minimal,
            processing_lag_days: 3,
            window_in_days: 1,
            streams: None,
            search_type: "web".into(),
            data_state: "final".into(),
            row_limit: 100,
            request_interval_ms: 0,
            max_api_retries: 3,
            url_inspection_enabled: false,
            url_list: vec![],
        };
        std::env::remove_var("SKIPPR_GOOGLE_ADS_FIXTURE_DIR");
        std::env::remove_var("GOOGLE_ADS_ACCESS_TOKEN");
        std::env::remove_var("GOOGLE_APPLICATION_CREDENTIALS");
        let plugin = DataSourceGoogleAdsPlugin::new(cfg).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt.block_on(plugin.auth_header()).expect_err("auth should fail");
        assert!(
            err.to_string().contains("access_token")
                || err.to_string().contains("credentials")
        );
        if let Some(path) = saved_gac {
            std::env::set_var("GOOGLE_APPLICATION_CREDENTIALS", path);
        }
    }

    #[test]
    fn discover_sync_does_not_store_checkpoints() {
        let _lock = env_lock();
        let fixture_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        std::env::set_var("SKIPPR_GOOGLE_ADS_FIXTURE_DIR", fixture_dir);
        std::env::set_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV, "discover");

        let mut cfg = test_config();
        cfg.end_date = Some("2025-06-01".into());
        cfg.stream_profile = StreamProfile::Minimal;
        let mut plugin = DataSourceGoogleAdsPlugin::new(cfg).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        rt.block_on(plugin.sync(ctx.clone())).expect("discover sync");
        assert!(ctx.checkpoint_stores.lock().unwrap().is_empty());

        std::env::remove_var("SKIPPR_GOOGLE_ADS_FIXTURE_DIR");
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);
    }

    use skippr_runtime_sdk::plugins::{
        OffsetValidationEntry, SourcePayloadTask, SourceSyncContext,
    };
    use skippr_runtime_sdk::protocol::RuntimeOffsetMaterializationHint;
    use skippr_runtime_sdk::source_compat::ThroughputMetrics;

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
