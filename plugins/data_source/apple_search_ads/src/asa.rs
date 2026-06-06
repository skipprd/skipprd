use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{Duration, NaiveDate, Utc};
use serde::Deserialize;
use serde_derive::Serialize;
use skippr_plugin_shared_api_source::{
    AppleAdsClientCredentialsAuth, DateWindowPlanner, RetryConfig, RetryableHttpClient,
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
use tokio::sync::Semaphore;
use tracing::{info, warn};

use crate::asa_api::{rows_from_report_body, AdGroupRef, AsaApiClient, CampaignRef};
use crate::streams::{
    resolve_streams, stream_needs_ad_group_enumeration, stream_needs_campaign_enumeration,
    AsaStreamDef, FanOut, ReportGrain, StreamProfile,
};

const CHECKPOINT_PAYLOAD_VERSION: u32 = 1;
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
struct AsaNamespaceCheckpoint {
    last_completed_date: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DataSourceAppleSearchAdsPluginConfig {
    pub org_id: String,
    pub client_id: String,
    pub team_id: String,
    pub key_id: String,
    #[serde(default)]
    pub private_key_path: Option<String>,
    #[serde(default)]
    pub private_key_pem: Option<String>,
    #[serde(default)]
    pub access_token: Option<String>,
    pub start_date: String,
    #[serde(default)]
    pub end_date: Option<String>,
    #[serde(default = "default_lookback_days")]
    pub lookback_days: u32,
    #[serde(default = "default_stream_profile")]
    pub stream_profile: StreamProfile,
    #[serde(default = "default_processing_lag_days")]
    pub processing_lag_days: u32,
    #[serde(default = "default_time_zone")]
    pub time_zone: String,
    #[serde(default = "default_return_records_with_no_metrics")]
    pub return_records_with_no_metrics: bool,
    #[serde(default = "default_max_concurrent_requests")]
    pub max_concurrent_requests: u32,
    #[serde(default)]
    pub streams: Option<Vec<String>>,
}

fn default_lookback_days() -> u32 {
    3
}

fn default_stream_profile() -> StreamProfile {
    StreamProfile::Full
}

fn default_processing_lag_days() -> u32 {
    1
}

fn default_time_zone() -> String {
    "UTC".into()
}

fn default_return_records_with_no_metrics() -> bool {
    true
}

fn default_max_concurrent_requests() -> u32 {
    8
}

struct EnumerationCache {
    campaigns: Vec<CampaignRef>,
    ad_groups_by_campaign: HashMap<i64, Vec<AdGroupRef>>,
}

pub struct DataSourceAppleSearchAdsPlugin {
    config: DataSourceAppleSearchAdsPluginConfig,
    http: RetryableHttpClient,
    auth: Option<AppleAdsClientCredentialsAuth>,
}

impl DataSourceAppleSearchAdsPlugin {
    pub fn new(config: DataSourceAppleSearchAdsPluginConfig) -> Result<Self, std::io::Error> {
        config.validate()?;
        let auth = Self::build_auth(&config)?;
        let http = RetryableHttpClient::new(RetryConfig::default());
        Ok(Self { config, http, auth })
    }

    fn build_auth(
        config: &DataSourceAppleSearchAdsPluginConfig,
    ) -> Result<Option<AppleAdsClientCredentialsAuth>, std::io::Error> {
        if config
            .access_token
            .as_deref()
            .filter(|t| !t.trim().is_empty())
            .is_some()
        {
            return Ok(None);
        }
        if std::env::var("SKIPPR_APPLE_SEARCH_ADS_FIXTURE_DIR")
            .map(|d| !d.trim().is_empty())
            .unwrap_or(false)
        {
            return Ok(None);
        }
        if let Some(pem) = config
            .private_key_pem
            .as_deref()
            .filter(|p| !p.trim().is_empty())
        {
            return AppleAdsClientCredentialsAuth::from_private_key_pem(
                &config.client_id,
                &config.team_id,
                &config.key_id,
                pem.as_bytes(),
            )
            .map(Some)
            .map_err(std::io::Error::other);
        }
        if let Some(path) = config
            .private_key_path
            .as_deref()
            .filter(|p| !p.trim().is_empty())
        {
            return AppleAdsClientCredentialsAuth::from_private_key_path(
                &config.client_id,
                &config.team_id,
                &config.key_id,
                path,
            )
            .map(Some)
            .map_err(std::io::Error::other);
        }
        if let Ok(path) = std::env::var("APPLE_SEARCH_ADS_PRIVATE_KEY_PATH") {
            if !path.trim().is_empty() {
                return AppleAdsClientCredentialsAuth::from_private_key_path(
                    &config.client_id,
                    &config.team_id,
                    &config.key_id,
                    &path,
                )
                .map(Some)
                .map_err(std::io::Error::other);
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Apple Search Ads requires private_key_path, private_key_pem, or access_token",
        ))
    }

    fn api_client(&self) -> AsaApiClient {
        let access_token = self
            .config
            .access_token
            .clone()
            .or_else(|| std::env::var("APPLE_SEARCH_ADS_ACCESS_TOKEN").ok());
        AsaApiClient::new(
            self.http.clone(),
            self.config.org_id.clone(),
            self.config.time_zone.clone(),
            self.config.return_records_with_no_metrics,
            self.auth.clone(),
            access_token,
        )
    }

    fn selected_streams(&self) -> Vec<&'static AsaStreamDef> {
        resolve_streams(self.config.stream_profile, self.config.streams.clone())
    }

    fn streams_for_run(&self, discover: bool) -> Vec<&'static AsaStreamDef> {
        if discover {
            resolve_streams(StreamProfile::Minimal, None)
        } else {
            self.selected_streams()
        }
    }

    fn namespace_contract(stream: &AsaStreamDef) -> SourceNamespaceContract {
        let mut primary_key = vec![
            FieldPath::single("org_id"),
            FieldPath::single("date"),
            FieldPath::single("campaign_id"),
        ];
        match stream.grain {
            ReportGrain::Campaign => {}
            ReportGrain::AdGroup | ReportGrain::Keyword | ReportGrain::SearchTerm => {
                primary_key.push(FieldPath::single("ad_group_id"));
            }
        }
        match stream.grain {
            ReportGrain::Campaign | ReportGrain::AdGroup | ReportGrain::SearchTerm => {}
            ReportGrain::Keyword => {
                primary_key.push(FieldPath::single("keyword_id"));
            }
        }
        if stream.grain == ReportGrain::SearchTerm {
            primary_key.push(FieldPath::single("search_term"));
        }
        SourceNamespaceContract {
            namespace: stream.namespace.to_string(),
            primary_key,
            cursor: Some(FieldPath::single("date")),
            partition_key: vec![FieldPath::single("date")],
            write_policy: WritePolicy::ReplacePartition,
            refresh_window: None,
            description: "Apple Search Ads mutable daily report".into(),
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
        format!("asa:{}:{}", self.config.org_id.trim(), namespace)
    }

    fn load_last_completed(ctx: &dyn SourceSyncContext, key: &str) -> Option<NaiveDate> {
        let Some(cp) = load_checkpoint_payload::<AsaNamespaceCheckpoint>(ctx, key) else {
            return None;
        };
        match Self::parse_date(&cp.last_completed_date) {
            Ok(date) => Some(date),
            Err(err) => {
                tracing::warn!("ASA ignoring corrupt checkpoint for {key}: {err}");
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
            &AsaNamespaceCheckpoint {
                last_completed_date: date.format("%Y-%m-%d").to_string(),
            },
        )
        .map_err(|e| std::io::Error::other(e.to_string()))?;
        ctx.store_checkpoint(key, &envelope)
            .map_err(std::io::Error::other)
    }

    async fn load_enumeration_cache(
        &self,
        api: &AsaApiClient,
        streams: &[&AsaStreamDef],
    ) -> Result<EnumerationCache, std::io::Error> {
        let needs_campaigns = streams.iter().any(|s| stream_needs_campaign_enumeration(s));
        let needs_ad_groups = streams.iter().any(|s| stream_needs_ad_group_enumeration(s));

        let campaigns = if needs_campaigns {
            api.list_campaigns().await?
        } else {
            Vec::new()
        };

        let mut ad_groups_by_campaign = HashMap::new();
        if needs_ad_groups {
            for campaign in &campaigns {
                let groups = api.list_ad_groups(campaign.id).await?;
                ad_groups_by_campaign.insert(campaign.id, groups);
            }
        }

        Ok(EnumerationCache {
            campaigns,
            ad_groups_by_campaign,
        })
    }

    fn submit_rows_for_date(
        &self,
        ctx: &dyn SourceSyncContext,
        stream: &AsaStreamDef,
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
        let offset_key = OffsetKey::new(stream.namespace, date.format("%Y-%m-%d").to_string());
        submit_payload_batches(
            ctx,
            vec![IngestBatch {
                offset_key,
                data: payload,
                bytes,
                offset_pos: None,
                source_uri: format!(
                    "apple-search-ads://org/{}/reports",
                    self.config.org_id.trim()
                ),
                namespace: Some(stream.namespace.to_string()),
                cdc_rows: None,
            }],
        )?;
        Ok(())
    }

    async fn sync_stream_date(
        &self,
        api: &AsaApiClient,
        stream: &AsaStreamDef,
        date: NaiveDate,
        cache: &EnumerationCache,
        semaphore: Arc<Semaphore>,
    ) -> Result<Vec<serde_json::Value>, std::io::Error> {
        let org_id = self.config.org_id.trim().to_string();
        match stream.fan_out {
            FanOut::None => {
                let body = api.fetch_report_all_pages(stream, date, None, None).await?;
                Ok(parse_report_rows(&body, stream.grain, &org_id))
            }
            FanOut::PerCampaign => {
                let mut all_rows = Vec::new();
                let mut handles = Vec::new();
                for campaign in &cache.campaigns {
                    let api = api.clone();
                    let stream = *stream;
                    let org_id = org_id.clone();
                    let permit = semaphore
                        .clone()
                        .acquire_owned()
                        .await
                        .map_err(|e| std::io::Error::other(e.to_string()))?;
                    let campaign_id = campaign.id;
                    handles.push(tokio::spawn(async move {
                        let _permit = permit;
                        let body = api
                            .fetch_report_all_pages(&stream, date, Some(campaign_id), None)
                            .await?;
                        Ok::<_, std::io::Error>(parse_report_rows(&body, stream.grain, &org_id))
                    }));
                }
                for handle in handles {
                    all_rows.extend(handle.await.map_err(std::io::Error::other)??);
                }
                Ok(all_rows)
            }
            FanOut::PerCampaignAdGroup => {
                let mut all_rows = Vec::new();
                let mut handles = Vec::new();
                for campaign in &cache.campaigns {
                    let groups = cache
                        .ad_groups_by_campaign
                        .get(&campaign.id)
                        .cloned()
                        .unwrap_or_default();
                    for ad_group in groups {
                        let api = api.clone();
                        let stream = *stream;
                        let org_id = org_id.clone();
                        let permit = semaphore
                            .clone()
                            .acquire_owned()
                            .await
                            .map_err(|e| std::io::Error::other(e.to_string()))?;
                        let campaign_id = campaign.id;
                        let ad_group_id = ad_group.id;
                        handles.push(tokio::spawn(async move {
                            let _permit = permit;
                            let body = api
                                .fetch_report_all_pages(
                                    &stream,
                                    date,
                                    Some(campaign_id),
                                    Some(ad_group_id),
                                )
                                .await?;
                            Ok::<_, std::io::Error>(parse_report_rows(&body, stream.grain, &org_id))
                        }));
                    }
                }
                for handle in handles {
                    all_rows.extend(handle.await.map_err(std::io::Error::other)??);
                }
                Ok(all_rows)
            }
        }
    }
}

impl DataSourceAppleSearchAdsPluginConfig {
    pub fn validate(&self) -> Result<(), std::io::Error> {
        if self.org_id.trim().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "org_id is required",
            ));
        }
        Ok(())
    }
}

pub(crate) fn parse_report_rows(
    body: &serde_json::Value,
    grain: ReportGrain,
    org_id: &str,
) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for row in rows_from_report_body(body) {
        let metadata = row
            .get("metadata")
            .cloned()
            .unwrap_or(serde_json::json!({}));
        let daily_slices = daily_metric_slices(&row);
        for slice in daily_slices {
            let mut record = serde_json::Map::new();
            record.insert(
                "org_id".into(),
                serde_json::Value::String(org_id.to_string()),
            );
            if let Some(date) = slice.get("date").and_then(|v| v.as_str()) {
                record.insert("date".into(), serde_json::Value::String(date.to_string()));
            }
            inject_grain_ids(&mut record, grain, &metadata);
            flatten_metrics_into(&mut record, &slice);
            flatten_metrics_into(&mut record, &metadata);
            normalize_spend_field(&mut record);
            out.push(serde_json::Value::Object(record));
        }
    }
    out
}

fn daily_metric_slices(row: &serde_json::Value) -> Vec<serde_json::Value> {
    if let Some(granularity) = row.get("granularity").and_then(|v| v.as_array()) {
        return granularity.clone();
    }
    if let Some(total) = row.get("total") {
        return vec![total.clone()];
    }
    Vec::new()
}

fn inject_grain_ids(
    record: &mut serde_json::Map<String, serde_json::Value>,
    grain: ReportGrain,
    metadata: &serde_json::Value,
) {
    if let Some(id) = metadata.get("campaignId").and_then(json_to_i64) {
        record.insert("campaign_id".into(), serde_json::json!(id));
    }
    if let Some(name) = metadata.get("campaignName").and_then(|v| v.as_str()) {
        record.insert(
            "campaign_name".into(),
            serde_json::Value::String(name.into()),
        );
    }
    if matches!(
        grain,
        ReportGrain::AdGroup | ReportGrain::Keyword | ReportGrain::SearchTerm
    ) {
        if let Some(id) = metadata.get("adGroupId").and_then(json_to_i64) {
            record.insert("ad_group_id".into(), serde_json::json!(id));
        }
        if let Some(name) = metadata.get("adGroupName").and_then(|v| v.as_str()) {
            record.insert(
                "ad_group_name".into(),
                serde_json::Value::String(name.into()),
            );
        }
    }
    if grain == ReportGrain::Keyword {
        if let Some(id) = metadata.get("keywordId").and_then(json_to_i64) {
            record.insert("keyword_id".into(), serde_json::json!(id));
        }
        if let Some(keyword) = metadata.get("keyword").and_then(|v| v.as_str()) {
            record.insert("keyword".into(), serde_json::Value::String(keyword.into()));
        }
    }
    if grain == ReportGrain::SearchTerm {
        let term = metadata
            .get("searchTermText")
            .or_else(|| metadata.get("searchTerm"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        if let Some(term) = term {
            record.insert("search_term".into(), serde_json::Value::String(term.into()));
        }
        if let Some(id) = metadata.get("keywordId").and_then(json_to_i64) {
            record.insert("keyword_id".into(), serde_json::json!(id));
        }
    }
}

fn json_to_i64(value: &serde_json::Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().map(|n| n as i64))
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
}

fn normalize_spend_field(record: &mut serde_json::Map<String, serde_json::Value>) {
    if record.contains_key("spend") {
        return;
    }
    let amount = record
        .get("localSpend")
        .and_then(|v| v.get("amount"))
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<f64>().ok());
    if let Some(spend) = amount {
        record.insert("spend".into(), serde_json::json!(spend));
    }
}

fn flatten_metrics_into(
    record: &mut serde_json::Map<String, serde_json::Value>,
    source: &serde_json::Value,
) {
    let Some(obj) = source.as_object() else {
        return;
    };
    for (key, value) in obj {
        if key == "date" {
            continue;
        }
        if matches!(
            key.as_str(),
            "campaignId"
                | "campaignName"
                | "adGroupId"
                | "adGroupName"
                | "keywordId"
                | "keyword"
                | "searchTermText"
        ) {
            continue;
        }
        record.insert(key.clone(), value.clone());
    }
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
impl DataSource for DataSourceAppleSearchAdsPlugin {
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
                .expect("invalid ASA namespace contract configuration");
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
                "ASA discover: sampling recent days only; full historical sync runs on skippr sync"
            );
            sample_start
        } else {
            configured_start
        };
        if end_date < start_date {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("ASA end date {end_date} is before start date {start_date}"),
            ));
        }

        let streams = self.streams_for_run(discover);
        if discover {
            info!(
                stream_count = streams.len(),
                stream_profile = "minimal",
                "ASA discover: sampling minimal streams and recent days; contracts reflect configured profile"
            );
        }

        let api = self.api_client();
        let enumeration = if discover {
            EnumerationCache {
                campaigns: Vec::new(),
                ad_groups_by_campaign: HashMap::new(),
            }
        } else {
            let needs_any = streams.iter().any(|s| s.fan_out != FanOut::None);
            if needs_any {
                let cache = self.load_enumeration_cache(&api, &streams).await?;
                if cache.campaigns.is_empty()
                    && streams.iter().any(|s| stream_needs_campaign_enumeration(s))
                {
                    warn!("ASA campaign list is empty; child streams may return no rows");
                }
                cache
            } else {
                EnumerationCache {
                    campaigns: Vec::new(),
                    ad_groups_by_campaign: HashMap::new(),
                }
            }
        };

        let semaphore = Arc::new(Semaphore::new(
            self.config.max_concurrent_requests.max(1) as usize
        ));
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

            for date in dates {
                let rows = self
                    .sync_stream_date(&api, stream, date, &enumeration, Arc::clone(&semaphore))
                    .await?;
                if !discover {
                    Self::store_last_completed(ctx.as_ref(), &checkpoint_key, date)?;
                }
                let date_key = date.format("%Y-%m-%d").to_string();
                let rows_for_day = group_rows_by_date(rows)
                    .get(&date_key)
                    .cloned()
                    .unwrap_or_default();
                self.submit_rows_for_date(ctx.as_ref(), stream, date, rows_for_day)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streams::{streams_for_profile, StreamProfile, FULL_STREAM_COUNT};
    use skippr_plugin_shared_api_source::DateWindow;

    use std::sync::Mutex;

    static ENV_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn env_test_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn test_config() -> DataSourceAppleSearchAdsPluginConfig {
        DataSourceAppleSearchAdsPluginConfig {
            org_id: "12345".into(),
            client_id: "client".into(),
            team_id: "team".into(),
            key_id: "key".into(),
            private_key_path: None,
            private_key_pem: None,
            access_token: Some("token".into()),
            start_date: "2024-01-01".into(),
            end_date: None,
            lookback_days: 3,
            stream_profile: StreamProfile::Full,
            processing_lag_days: 1,
            time_zone: "UTC".into(),
            return_records_with_no_metrics: true,
            max_concurrent_requests: 8,
            streams: None,
        }
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
        let plugin = DataSourceAppleSearchAdsPlugin::new(test_config()).unwrap();
        let discover_streams = plugin.streams_for_run(true);
        assert_eq!(discover_streams.len(), 1);
        assert_eq!(plugin.streams_for_run(false).len(), FULL_STREAM_COUNT);
        assert!(
            discover_streams.iter().all(|s| s.fan_out == FanOut::None),
            "discover must not select fan-out streams"
        );
    }

    #[test]
    fn runtime_is_discover_mode_reads_child_env() {
        let _lock = env_test_lock();
        std::env::set_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV, "discover");
        assert!(runtime_is_discover_mode());
        std::env::set_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV, "sync");
        assert!(!runtime_is_discover_mode());
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);
    }

    #[test]
    fn namespace_contracts_use_replace_partition() {
        let plugin = DataSourceAppleSearchAdsPlugin::new(test_config()).unwrap();
        let contracts = plugin.source_namespace_contracts();
        assert_eq!(contracts.len(), FULL_STREAM_COUNT);
        assert!(contracts
            .iter()
            .all(|c| c.write_policy == WritePolicy::ReplacePartition));
    }

    #[test]
    fn profile_stream_counts() {
        assert_eq!(streams_for_profile(StreamProfile::Minimal).len(), 1);
        assert_eq!(streams_for_profile(StreamProfile::Standard).len(), 2);
        assert_eq!(streams_for_profile(StreamProfile::Full).len(), 4);
    }

    #[test]
    fn parses_search_term_report_fixture() {
        let fixture_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        let body: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(format!("{fixture_dir}/search_term_report.json")).unwrap(),
        )
        .unwrap();
        let rows = parse_report_rows(&body, ReportGrain::SearchTerm, "999");
        assert_eq!(rows[0]["search_term"], "running shoes");
    }

    #[test]
    fn parses_campaign_report_fixture() {
        let fixture_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        let body: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(format!("{fixture_dir}/campaign_report.json")).unwrap(),
        )
        .unwrap();
        let rows = parse_report_rows(&body, ReportGrain::Campaign, "999");
        assert!(!rows.is_empty());
        assert_eq!(rows[0]["org_id"], "999");
        assert_eq!(rows[0]["date"], "2024-01-01");
        assert_eq!(rows[0]["campaign_id"], 1001);
    }

    #[test]
    fn parses_ad_group_keyword_and_search_term_fixtures() {
        let fixture_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        for (file, grain) in [
            ("ad_group_report.json", ReportGrain::AdGroup),
            ("keyword_report.json", ReportGrain::Keyword),
            ("search_term_report.json", ReportGrain::SearchTerm),
        ] {
            let body: serde_json::Value = serde_json::from_str(
                &std::fs::read_to_string(format!("{fixture_dir}/{file}")).unwrap(),
            )
            .unwrap();
            let rows = parse_report_rows(&body, grain, "999");
            assert!(!rows.is_empty(), "expected rows in {file}");
            assert_eq!(rows[0]["org_id"], "999");
            assert_eq!(rows[0]["date"], "2024-01-01");
        }
    }

    #[test]
    fn search_term_contract_primary_key_includes_search_term() {
        let stream = streams_for_profile(StreamProfile::Full)
            .into_iter()
            .find(|s| s.grain == ReportGrain::SearchTerm)
            .unwrap();
        let contract = DataSourceAppleSearchAdsPlugin::namespace_contract(stream);
        let pk: Vec<String> = contract.primary_key.iter().map(|f| f.dotted()).collect();
        assert!(pk.contains(&"search_term".to_string()));
    }

    #[test]
    fn asa_checkpoint_roundtrip_envelope() {
        let cp = AsaNamespaceCheckpoint {
            last_completed_date: "2024-03-01".into(),
        };
        let envelope = CheckpointEnvelope::from_payload(
            CheckpointAuthority::AdvisoryHint,
            CheckpointKind::SourceResume,
            CHECKPOINT_PAYLOAD_VERSION,
            &cp,
        )
        .expect("checkpoint envelope");
        let decoded: AsaNamespaceCheckpoint = envelope.into_payload().expect("payload");
        assert_eq!(decoded.last_completed_date, "2024-03-01");
    }

    #[test]
    fn loads_fixture_dir_for_campaign_rows() {
        let _lock = env_test_lock();
        let fixture_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        std::env::set_var("SKIPPR_APPLE_SEARCH_ADS_FIXTURE_DIR", fixture_dir);
        let plugin = DataSourceAppleSearchAdsPlugin::new(test_config()).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let api = plugin.api_client();
        let stream = streams_for_profile(StreamProfile::Minimal)[0];
        let date = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
        let cache = EnumerationCache {
            campaigns: Vec::new(),
            ad_groups_by_campaign: HashMap::new(),
        };
        let sem = Arc::new(Semaphore::new(1));
        let rows = rt
            .block_on(plugin.sync_stream_date(&api, stream, date, &cache, sem))
            .expect("fixture sync");
        assert!(!rows.is_empty());
        std::env::remove_var("SKIPPR_APPLE_SEARCH_ADS_FIXTURE_DIR");
    }

    #[test]
    fn config_defaults_and_validation_succeed() {
        let cfg: DataSourceAppleSearchAdsPluginConfig = serde_json::from_value(serde_json::json!({
            "org_id": "12345",
            "client_id": "client",
            "team_id": "team",
            "key_id": "key",
            "access_token": "token",
            "start_date": "2024-01-01"
        }))
        .expect("deserialize");
        assert_eq!(cfg.stream_profile, StreamProfile::Full);
        assert_eq!(cfg.lookback_days, 3);
        cfg.validate().expect("valid org_id");
        DataSourceAppleSearchAdsPlugin::new(cfg).expect("plugin init");
    }

    #[test]
    fn namespace_contracts_use_mutable_report_semantics() {
        let plugin = DataSourceAppleSearchAdsPlugin::new(test_config()).unwrap();
        let contracts = plugin.source_namespace_contracts();
        assert!(contracts.iter().all(|c| {
            c.semantics == Some(SourceSemantics::MutableReport)
                && c.partition_key.iter().any(|f| f.dotted() == "date")
        }));
    }

    #[test]
    fn contract_primary_key_includes_grain_dimensions() {
        use crate::streams::CURATED_STREAMS;

        for stream in CURATED_STREAMS {
            let contract = DataSourceAppleSearchAdsPlugin::namespace_contract(stream);
            let pk: Vec<String> = contract.primary_key.iter().map(|f| f.dotted()).collect();
            assert!(pk.contains(&"org_id".to_string()));
            assert!(pk.contains(&"date".to_string()));
            assert!(pk.contains(&"campaign_id".to_string()));
            match stream.grain {
                ReportGrain::Campaign => {}
                ReportGrain::AdGroup | ReportGrain::Keyword | ReportGrain::SearchTerm => {
                    assert!(
                        pk.contains(&"ad_group_id".to_string()),
                        "{}",
                        stream.namespace
                    );
                }
            }
            if stream.grain == ReportGrain::Keyword {
                assert!(pk.contains(&"keyword_id".to_string()));
            }
            if stream.grain == ReportGrain::SearchTerm {
                assert!(pk.contains(&"search_term".to_string()));
            }
        }
    }

    #[test]
    fn invalid_start_date_rejected() {
        let err = DataSourceAppleSearchAdsPlugin::parse_date("not-a-date").unwrap_err();
        assert!(err.to_string().contains("invalid date"));
    }

    #[test]
    fn end_date_before_start_date_rejected_on_sync() {
        let _lock = env_test_lock();
        let fixture_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        std::env::set_var("SKIPPR_APPLE_SEARCH_ADS_FIXTURE_DIR", fixture_dir);
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);

        let mut cfg = test_config();
        cfg.start_date = "2024-12-01".into();
        cfg.end_date = Some("2024-01-01".into());
        let mut plugin = DataSourceAppleSearchAdsPlugin::new(cfg).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        let err = rt.block_on(plugin.sync(ctx)).expect_err("end before start");
        assert!(err.to_string().contains("before start date"));

        std::env::remove_var("SKIPPR_APPLE_SEARCH_ADS_FIXTURE_DIR");
    }

    #[test]
    fn missing_credentials_returns_clear_error() {
        let _lock = env_test_lock();
        let cfg = DataSourceAppleSearchAdsPluginConfig {
            org_id: "12345".into(),
            client_id: "client".into(),
            team_id: "team".into(),
            key_id: "key".into(),
            private_key_path: None,
            private_key_pem: None,
            access_token: None,
            start_date: "2024-01-01".into(),
            end_date: None,
            lookback_days: 3,
            stream_profile: StreamProfile::Minimal,
            processing_lag_days: 1,
            time_zone: "UTC".into(),
            return_records_with_no_metrics: true,
            max_concurrent_requests: 8,
            streams: None,
        };
        std::env::remove_var("SKIPPR_APPLE_SEARCH_ADS_FIXTURE_DIR");
        std::env::remove_var("APPLE_SEARCH_ADS_PRIVATE_KEY_PATH");
        std::env::remove_var("APPLE_SEARCH_ADS_ACCESS_TOKEN");
        let err = match DataSourceAppleSearchAdsPlugin::new(cfg) {
            Err(err) => err,
            Ok(_) => panic!("expected missing credentials error"),
        };
        assert!(err.to_string().contains("private_key_path"));
    }

    #[test]
    fn empty_org_id_validation_fails() {
        let mut cfg = test_config();
        cfg.org_id = "  ".into();
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("org_id is required"));
    }

    #[test]
    fn jwt_build_fails_with_bad_pem() {
        use skippr_plugin_shared_api_source::AppleAdsClientCredentialsAuth;

        let auth = AppleAdsClientCredentialsAuth::from_private_key_pem(
            "client",
            "team",
            "key",
            b"not-a-valid-pem",
        )
        .expect("pem stored");
        let err = auth.build_client_secret_jwt(1_700_000_000).unwrap_err();
        assert!(!err.is_empty());
    }

    #[test]
    fn parse_empty_or_malformed_report_body_returns_empty_rows() {
        assert!(parse_report_rows(&serde_json::json!({}), ReportGrain::Campaign, "1").is_empty());
        assert!(parse_report_rows(&serde_json::json!(null), ReportGrain::Campaign, "1").is_empty());
        assert!(parse_report_rows(
            &serde_json::json!({"data": {"reportingDataResponse": {"row": "not-array"}}}),
            ReportGrain::Campaign,
            "1"
        )
        .is_empty());
    }

    #[test]
    fn search_term_row_missing_term_omits_search_term_field() {
        let body = serde_json::json!({
            "data": {
                "reportingDataResponse": {
                    "row": [{
                        "metadata": { "campaignId": 1001 },
                        "granularity": [{ "date": "2024-01-01", "impressions": 1 }]
                    }]
                }
            }
        });
        let rows = parse_report_rows(&body, ReportGrain::SearchTerm, "999");
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].as_object().unwrap().contains_key("search_term"));
    }

    #[test]
    fn discover_sync_does_not_store_checkpoints() {
        let _lock = env_test_lock();
        let fixture_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        std::env::set_var("SKIPPR_APPLE_SEARCH_ADS_FIXTURE_DIR", fixture_dir);
        std::env::set_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV, "discover");

        let mut cfg = test_config();
        cfg.stream_profile = StreamProfile::Minimal;
        cfg.start_date = "2024-01-01".into();
        cfg.end_date = Some("2024-01-01".into());
        let mut plugin = DataSourceAppleSearchAdsPlugin::new(cfg).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        rt.block_on(plugin.sync(ctx.clone()))
            .expect("discover sync");
        assert!(
            ctx.checkpoint_stores.lock().unwrap().is_empty(),
            "discover must not persist checkpoints"
        );

        std::env::remove_var("SKIPPR_APPLE_SEARCH_ADS_FIXTURE_DIR");
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);
    }

    #[test]
    fn sync_mode_stores_checkpoints() {
        let _lock = env_test_lock();
        let fixture_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        std::env::set_var("SKIPPR_APPLE_SEARCH_ADS_FIXTURE_DIR", fixture_dir);
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);

        let mut cfg = test_config();
        cfg.stream_profile = StreamProfile::Minimal;
        cfg.start_date = "2024-01-01".into();
        cfg.end_date = Some("2024-01-01".into());
        let mut plugin = DataSourceAppleSearchAdsPlugin::new(cfg).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        rt.block_on(plugin.sync(ctx.clone())).expect("sync");
        assert!(
            !ctx.checkpoint_stores.lock().unwrap().is_empty(),
            "sync should persist checkpoints"
        );

        std::env::remove_var("SKIPPR_APPLE_SEARCH_ADS_FIXTURE_DIR");
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
