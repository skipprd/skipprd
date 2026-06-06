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

use crate::ga4_api::{
    authorization_header, is_invalid_dimension_metric_error, normalize_property_id,
    run_report_range,
};
use crate::streams::{resolve_streams, Ga4StreamDef, StreamProfile};

const CHECKPOINT_PAYLOAD_VERSION: u32 = 1;

/// Temporary discover sampling: last N days × minimal stream profile only (no customer config).
const DISCOVER_SAMPLE_DAYS: u32 = 3;

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
struct Ga4NamespaceCheckpoint {
    last_completed_date: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DataSourceGoogleAnalyticsPluginConfig {
    pub property_id: String,
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
    #[serde(default = "default_keep_empty_rows")]
    pub keep_empty_rows: bool,
    #[serde(default = "default_processing_lag_days")]
    pub processing_lag_days: u32,
    #[serde(default = "default_window_in_days")]
    pub window_in_days: u32,
    #[serde(default)]
    pub streams: Option<Vec<String>>,
    /// Pause between successful Data API runReport calls (reduces 429 quota errors).
    #[serde(default = "default_request_interval_ms")]
    pub request_interval_ms: u64,
    /// Per-request retries on HTTP 429 / 5xx (exponential backoff in the plugin HTTP client).
    #[serde(default = "default_max_api_retries")]
    pub max_api_retries: u32,
}

fn default_lookback_days() -> u32 {
    3
}

fn default_stream_profile() -> StreamProfile {
    StreamProfile::Full
}

fn default_keep_empty_rows() -> bool {
    true
}

fn default_processing_lag_days() -> u32 {
    1
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

impl DataSourceGoogleAnalyticsPluginConfig {
    pub fn validate(&self) -> Result<(), std::io::Error> {
        if self.window_in_days < 1 || self.window_in_days > 364 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "window_in_days must be between 1 and 364",
            ));
        }
        Ok(())
    }
}

pub struct DataSourceGoogleAnalyticsPlugin {
    config: DataSourceGoogleAnalyticsPluginConfig,
    http: RetryableHttpClient,
    oauth: Option<OAuth2RefreshTokenAuth>,
}

impl DataSourceGoogleAnalyticsPlugin {
    pub fn new(config: DataSourceGoogleAnalyticsPluginConfig) -> Result<Self, std::io::Error> {
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
        config: &DataSourceGoogleAnalyticsPluginConfig,
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

    fn selected_streams(&self) -> Vec<&'static Ga4StreamDef> {
        resolve_streams(self.config.stream_profile, self.config.streams.clone())
    }

    fn streams_for_run(&self, discover: bool) -> Vec<&'static Ga4StreamDef> {
        if discover {
            resolve_streams(StreamProfile::Minimal, None)
        } else {
            self.selected_streams()
        }
    }

    fn namespace_contract(stream: &Ga4StreamDef) -> SourceNamespaceContract {
        let mut primary_key = vec![FieldPath::single("property_id"), FieldPath::single("date")];
        for dim in stream.dimensions {
            if *dim != "date" {
                primary_key.push(FieldPath::single(*dim));
            }
        }
        SourceNamespaceContract {
            namespace: stream.namespace.to_string(),
            primary_key,
            cursor: Some(FieldPath::single("date")),
            partition_key: vec![FieldPath::single("date")],
            write_policy: WritePolicy::ReplacePartition,
            refresh_window: None,
            description: "GA4 Data API mutable daily report".into(),
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
            "ga4:{}:{}",
            normalize_property_id(&self.config.property_id),
            namespace
        )
    }

    fn load_last_completed(ctx: &dyn SourceSyncContext, key: &str) -> Option<NaiveDate> {
        let Some(cp) = load_checkpoint_payload::<Ga4NamespaceCheckpoint>(ctx, key) else {
            return None;
        };
        match Self::parse_date(&cp.last_completed_date) {
            Ok(date) => Some(date),
            Err(err) => {
                tracing::warn!("GA4 ignoring corrupt checkpoint for {key}: {err}");
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
            &Ga4NamespaceCheckpoint {
                last_completed_date: date.format("%Y-%m-%d").to_string(),
            },
        )
        .map_err(|e| std::io::Error::other(e.to_string()))?;
        ctx.store_checkpoint(key, &envelope)
            .map_err(std::io::Error::other)
    }

    async fn auth_header(&self) -> Result<String, std::io::Error> {
        authorization_header(
            self.config.access_token.as_deref(),
            self.oauth.as_ref(),
            self.config.service_account_json_path.as_deref(),
        )
        .await
    }

    async fn rows_for_range(
        &self,
        stream: &Ga4StreamDef,
        start: NaiveDate,
        end: NaiveDate,
    ) -> Result<Vec<serde_json::Value>, std::io::Error> {
        if let Ok(fixture_dir) = std::env::var("SKIPPR_GA4_FIXTURE_DIR") {
            let mut all_rows = Vec::new();
            let mut cursor = start;
            while cursor <= end {
                let path = format!(
                    "{}/{}_{}.json",
                    fixture_dir.trim_end_matches('/'),
                    stream.namespace.replace('.', "_"),
                    cursor.format("%Y%m%d")
                );
                if let Ok(bytes) = std::fs::read(&path) {
                    let body: serde_json::Value = serde_json::from_slice(&bytes)
                        .map_err(|e| std::io::Error::other(e.to_string()))?;
                    all_rows.extend(parse_ga4_report_rows(
                        &body,
                        stream,
                        &self.config.property_id,
                    ));
                }
                cursor += Duration::days(1);
            }
            if !all_rows.is_empty() || start == end {
                return Ok(all_rows);
            }
        }

        let auth_header = self.auth_header().await?;
        let body = run_report_range(
            &self.http,
            &auth_header,
            &self.config.property_id,
            start,
            end,
            stream.dimensions,
            stream.metrics,
            self.config.keep_empty_rows,
        )
        .await?;
        Ok(parse_ga4_report_rows(
            &body,
            stream,
            &self.config.property_id,
        ))
    }

    /// One host ingest round-trip for all partition days in a chunk (critical when window_in_days > 1).
    fn submit_rows_grouped(
        &self,
        ctx: &dyn SourceSyncContext,
        stream: &Ga4StreamDef,
        grouped: &HashMap<String, Vec<serde_json::Value>>,
    ) -> Result<(), std::io::Error> {
        let property_id = normalize_property_id(&self.config.property_id);
        let source_uri = format!("ga4://properties/{property_id}/reports");
        let mut batches = Vec::with_capacity(grouped.len());
        for (date_key, rows) in grouped {
            if rows.is_empty() {
                continue;
            }
            let payload = rows
                .iter()
                .map(|row| serde_json::to_string(row))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| std::io::Error::other(e.to_string()))?
                .join("\n");
            let bytes = payload.len();
            batches.push(IngestBatch {
                offset_key: OffsetKey::new(stream.namespace, date_key.clone()),
                data: payload,
                bytes,
                offset_pos: None,
                source_uri: source_uri.clone(),
                namespace: Some(stream.namespace.to_string()),
                cdc_rows: None,
            });
        }
        if batches.is_empty() {
            return Ok(());
        }
        submit_payload_batches(ctx, batches)?;
        Ok(())
    }
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

pub(crate) fn parse_ga4_report_rows(
    body: &serde_json::Value,
    stream: &Ga4StreamDef,
    property_id: &str,
) -> Vec<serde_json::Value> {
    let rows = body
        .pointer("/rows")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    let property = normalize_property_id(property_id);
    for row in rows {
        let dimension_values = row
            .get("dimensionValues")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let metric_values = row
            .get("metricValues")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut record = serde_json::Map::new();
        record.insert(
            "property_id".into(),
            serde_json::Value::String(property.clone()),
        );
        for (idx, dim_name) in stream.dimensions.iter().enumerate() {
            let value = dimension_values
                .get(idx)
                .and_then(|v| v.get("value"))
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let formatted = if *dim_name == "date" && value.len() == 8 {
                format!("{}-{}-{}", &value[0..4], &value[4..6], &value[6..8])
            } else {
                value.to_string()
            };
            record.insert((*dim_name).into(), serde_json::Value::String(formatted));
        }
        for (idx, metric_name) in stream.metrics.iter().enumerate() {
            let value = metric_values
                .get(idx)
                .and_then(|v| v.get("value"))
                .and_then(|v| v.as_str())
                .unwrap_or("0");
            let numeric = value.parse::<f64>().unwrap_or(0.0);
            record.insert((*metric_name).into(), serde_json::json!(numeric));
        }
        out.push(serde_json::Value::Object(record));
    }
    out
}

#[async_trait]
impl DataSource for DataSourceGoogleAnalyticsPlugin {
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
                .expect("invalid GA4 namespace contract configuration");
        }
        contracts
    }

    async fn sync(&mut self, ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error> {
        self.config.validate()?;
        if self.config.window_in_days > 1 {
            warn!(
                window_in_days = self.config.window_in_days,
                "GA4 window_in_days > 1 may cause Data API sampling; prefer window_in_days=1 with \
                 lookback_days and replace_partition on scheduled syncs"
            );
        }

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
                "GA4 discover: sampling recent days only; full historical sync runs on skippr sync"
            );
            sample_start
        } else {
            configured_start
        };
        if end_date < start_date {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("GA4 end date {end_date} is before start date {start_date}"),
            ));
        }

        let streams = self.streams_for_run(discover);
        if discover {
            info!(
                stream_count = streams.len(),
                stream_profile = "minimal",
                "GA4 discover: sampling minimal stream profile for API calls; namespace contracts still reflect configured profile"
            );
        }

        let planner = DateWindowPlanner {
            lookback_days: if discover {
                0
            } else {
                self.config.lookback_days
            },
        };

        for stream in streams {
            let checkpoint_key = self.checkpoint_key(stream.namespace);
            let last_completed = if discover {
                None
            } else {
                Self::load_last_completed(ctx.as_ref(), &checkpoint_key)
            };
            let window = planner.plan(start_date, last_completed, end_date);
            let dates = DateWindowPlanner::dates_inclusive(&window);
            let chunks = chunk_dates(&dates, self.config.window_in_days);

            match last_completed {
                Some(cp) => info!(
                    namespace = stream.namespace,
                    last_completed = %cp,
                    effective_start = %window.start,
                    effective_end = %window.end,
                    lookback_days = planner.lookback_days,
                    api_calls = chunks.len(),
                    "GA4 resuming namespace from stored checkpoint (one runReport per date chunk; no API pagination)"
                ),
                None => info!(
                    namespace = stream.namespace,
                    effective_start = %window.start,
                    effective_end = %window.end,
                    api_calls = chunks.len(),
                    "GA4 no checkpoint for namespace; syncing full configured window"
                ),
            }

            for (chunk_start, chunk_end) in chunks {
                let mut chunk_dates_list = Vec::new();
                let mut cursor = chunk_start;
                while cursor <= chunk_end {
                    chunk_dates_list.push(cursor);
                    cursor += Duration::days(1);
                }
                info!(
                    namespace = stream.namespace,
                    chunk_start = %chunk_start,
                    chunk_end = %chunk_end,
                    day_count = chunk_dates_list.len(),
                    request_interval_ms = self.config.request_interval_ms,
                    "GA4 runReport: fetching date chunk"
                );
                match self.rows_for_range(stream, chunk_start, chunk_end).await {
                    Ok(rows) => {
                        info!(
                            namespace = stream.namespace,
                            chunk_start = %chunk_start,
                            chunk_end = %chunk_end,
                            row_count = rows.len(),
                            "GA4 runReport: chunk succeeded"
                        );
                        let grouped = group_rows_by_date(rows);
                        self.submit_rows_grouped(ctx.as_ref(), stream, &grouped)?;
                        if !discover {
                            for date in chunk_dates_list {
                                Self::store_last_completed(ctx.as_ref(), &checkpoint_key, date)?;
                            }
                        }
                        if self.config.request_interval_ms > 0 {
                            sleep(TokioDuration::from_millis(self.config.request_interval_ms))
                                .await;
                        }
                    }
                    Err(err) if stream.optional && is_invalid_dimension_metric_error(&err) => {
                        warn!("GA4 skipping optional stream {}: {err}", stream.namespace);
                        break;
                    }
                    Err(err) => return Err(err),
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streams::{streams_for_profile, StreamProfile, FULL_STREAM_COUNT};

    fn test_config() -> DataSourceGoogleAnalyticsPluginConfig {
        DataSourceGoogleAnalyticsPluginConfig {
            property_id: "123".into(),
            access_token: Some("token".into()),
            oauth_token_url: None,
            oauth_client_id: None,
            oauth_client_secret: None,
            oauth_refresh_token: None,
            service_account_json_path: None,
            start_date: "2024-01-01".into(),
            end_date: None,
            lookback_days: 3,
            stream_profile: StreamProfile::Full,
            keep_empty_rows: true,
            processing_lag_days: 1,
            window_in_days: 1,
            streams: None,
            request_interval_ms: 300,
            max_api_retries: 12,
        }
    }

    #[test]
    fn accuracy_first_config_defaults() {
        let cfg: DataSourceGoogleAnalyticsPluginConfig =
            serde_json::from_value(serde_json::json!({
                "property_id": "1",
                "start_date": "2024-01-01"
            }))
            .expect("deserialize");
        assert_eq!(cfg.stream_profile, StreamProfile::Full);
        assert!(cfg.keep_empty_rows);
        assert_eq!(cfg.lookback_days, 3);
        assert_eq!(cfg.processing_lag_days, 1);
        assert_eq!(cfg.window_in_days, 1);
    }

    #[test]
    fn discover_sample_window_is_last_three_days_inclusive() {
        let end = NaiveDate::from_ymd_opt(2026, 5, 25).unwrap();
        let (start, end_out) = discover_sample_date_window(end);
        assert_eq!(end_out, end);
        assert_eq!(start, NaiveDate::from_ymd_opt(2026, 5, 23).unwrap());
        assert_eq!(
            DateWindowPlanner::dates_inclusive(&DateWindow {
                start,
                end: end_out
            })
            .len(),
            DISCOVER_SAMPLE_DAYS as usize
        );
    }

    #[test]
    fn discover_mode_uses_minimal_streams_only() {
        let plugin = DataSourceGoogleAnalyticsPlugin::new(test_config()).unwrap();
        let discover_streams = plugin.streams_for_run(true);
        let sync_streams = plugin.streams_for_run(false);
        assert_eq!(discover_streams.len(), 4);
        assert_eq!(sync_streams.len(), FULL_STREAM_COUNT);
    }

    #[test]
    fn runtime_is_discover_mode_reads_child_env() {
        std::env::set_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV, "discover");
        assert!(runtime_is_discover_mode());
        std::env::set_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV, "sync");
        assert!(!runtime_is_discover_mode());
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);
        assert!(!runtime_is_discover_mode());
    }

    #[test]
    fn parses_fixture_report_rows() {
        let body = serde_json::json!({
            "rows": [{
                "dimensionValues": [{"value": "20240102"}, {"value": "Organic Search"}],
                "metricValues": [{"value": "10"}, {"value": "5"}]
            }]
        });
        let stream = streams_for_profile(StreamProfile::Minimal)[0];
        let rows = parse_ga4_report_rows(&body, stream, "properties/123");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["date"], "2024-01-02");
        assert_eq!(rows[0]["sessions"], serde_json::json!(10.0));
    }

    #[test]
    fn namespace_contracts_use_replace_partition() {
        let plugin = DataSourceGoogleAnalyticsPlugin::new(test_config()).unwrap();
        let contracts = plugin.source_namespace_contracts();
        assert_eq!(contracts.len(), FULL_STREAM_COUNT);
        assert!(contracts
            .iter()
            .all(|c| c.write_policy == WritePolicy::ReplacePartition));
    }

    #[test]
    fn chunk_dates_single_day_windows() {
        let dates = vec![
            NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(),
            NaiveDate::from_ymd_opt(2024, 1, 2).unwrap(),
            NaiveDate::from_ymd_opt(2024, 1, 3).unwrap(),
        ];
        let chunks = chunk_dates(&dates, 1);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0], (dates[0], dates[0]));
    }

    #[test]
    fn chunk_dates_multi_day_window() {
        let dates: Vec<NaiveDate> = (1..=5)
            .map(|d| NaiveDate::from_ymd_opt(2024, 1, d).unwrap())
            .collect();
        let chunks = chunk_dates(&dates, 3);
        assert_eq!(chunks.len(), 2);
        assert_eq!(
            chunks[0],
            (
                NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(),
                NaiveDate::from_ymd_opt(2024, 1, 3).unwrap()
            )
        );
    }

    #[test]
    fn group_rows_by_date_splits_multi_day_response() {
        let rows = vec![
            serde_json::json!({"date": "2024-01-01", "sessions": 1}),
            serde_json::json!({"date": "2024-01-02", "sessions": 2}),
            serde_json::json!({"date": "2024-01-01", "sessions": 3}),
        ];
        let grouped = group_rows_by_date(rows);
        assert_eq!(grouped.get("2024-01-01").unwrap().len(), 2);
        assert_eq!(grouped.get("2024-01-02").unwrap().len(), 1);
    }

    #[test]
    fn selected_streams_respects_explicit_filter() {
        let mut cfg = test_config();
        cfg.streams = Some(vec!["google_analytics.events_daily".into()]);
        let plugin = DataSourceGoogleAnalyticsPlugin::new(cfg).unwrap();
        let selected = plugin.selected_streams();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].namespace, "google_analytics.events_daily");
    }

    #[test]
    fn parses_report_rows_for_each_curated_stream() {
        use crate::streams::CURATED_STREAMS;

        for stream in CURATED_STREAMS {
            let dimension_values: Vec<serde_json::Value> = stream
                .dimensions
                .iter()
                .enumerate()
                .map(|(idx, name)| {
                    let value = if *name == "date" {
                        "20240101"
                    } else {
                        "sample"
                    };
                    let _ = idx;
                    serde_json::json!({"value": value})
                })
                .collect();
            let metric_values: Vec<serde_json::Value> = stream
                .metrics
                .iter()
                .map(|_| serde_json::json!({"value": "1"}))
                .collect();
            let body = serde_json::json!({
                "rows": [{
                    "dimensionValues": dimension_values,
                    "metricValues": metric_values
                }]
            });
            let rows = parse_ga4_report_rows(&body, stream, "123");
            assert_eq!(rows.len(), 1, "stream {}", stream.namespace);
            assert_eq!(rows[0]["date"], "2024-01-01");
            assert_eq!(rows[0]["property_id"], "123");
            for dim in stream.dimensions {
                assert!(
                    rows[0].get(*dim).is_some(),
                    "{} missing {dim}",
                    stream.namespace
                );
            }
        }
    }

    #[test]
    fn contract_primary_key_includes_breakdown_dimensions() {
        use crate::streams::CURATED_STREAMS;

        for stream in CURATED_STREAMS {
            let contract = DataSourceGoogleAnalyticsPlugin::namespace_contract(stream);
            let pk: Vec<String> = contract.primary_key.iter().map(|f| f.dotted()).collect();
            assert!(pk.contains(&"property_id".to_string()));
            assert!(pk.contains(&"date".to_string()));
            for dim in stream.dimensions {
                if *dim != "date" {
                    assert!(
                        pk.contains(&(*dim).to_string()),
                        "{} pk missing {dim}",
                        stream.namespace
                    );
                }
            }
        }
    }

    #[test]
    fn loads_skippr_ga4_fixture_dir() {
        use crate::streams::CURATED_STREAMS;

        let fixture_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        std::env::set_var("SKIPPR_GA4_FIXTURE_DIR", fixture_dir);
        let stream = CURATED_STREAMS
            .iter()
            .find(|s| s.namespace == "google_analytics.events_daily")
            .unwrap();
        let plugin = DataSourceGoogleAnalyticsPlugin::new(test_config()).unwrap();
        let date = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let rows = rt
            .block_on(plugin.rows_for_range(stream, date, date))
            .expect("fixture rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["eventName"], "sample");
        std::env::remove_var("SKIPPR_GA4_FIXTURE_DIR");
    }

    #[test]
    fn ga4_checkpoint_roundtrip_envelope() {
        let cp = Ga4NamespaceCheckpoint {
            last_completed_date: "2024-03-01".into(),
        };
        let envelope = CheckpointEnvelope::from_payload(
            CheckpointAuthority::AdvisoryHint,
            CheckpointKind::SourceResume,
            CHECKPOINT_PAYLOAD_VERSION,
            &cp,
        )
        .expect("checkpoint envelope");
        let decoded: Ga4NamespaceCheckpoint = envelope.into_payload().expect("payload");
        assert_eq!(decoded.last_completed_date, "2024-03-01");
    }
}
