use skippr_runtime_sdk::SkipprConfig;
use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{Duration, NaiveDate, Utc};
use serde::Deserialize;
use serde_derive::Serialize;
use skippr_plugin_shared_api_source::{
    BearerAuth, DateWindowPlanner, OAuth2RefreshTokenAuth, RetryConfig, RetryableHttpClient,
    StaticBearerAuth,
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
use tracing::info;

use crate::meta_api::{
    api_version_or_default, normalize_ad_account_id, rows_from_insights_body, MetaInsightsApiClient,
};
use crate::streams::{resolve_streams, InsightsLevel, MetaStreamDef, StreamProfile};

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
struct MetaNamespaceCheckpoint {
    last_completed_date: String,
}

#[derive(Debug, Clone, Deserialize, SkipprConfig)]
pub struct DataSourceMetaInstagramAdsPluginConfig {
    pub ad_account_id: String,
    #[serde(default)]
    #[skippr(secret)]
    pub access_token: Option<String>,
    #[serde(default)]
    #[skippr(not_secret)]
    pub oauth_token_url: Option<String>,
    #[serde(default)]
    pub oauth_client_id: Option<String>,
    #[serde(default)]
    #[skippr(secret)]
    pub oauth_client_secret: Option<String>,
    #[serde(default)]
    #[skippr(secret)]
    pub oauth_refresh_token: Option<String>,
    #[serde(default)]
    pub api_version: Option<String>,
    pub start_date: String,
    #[serde(default)]
    pub end_date: Option<String>,
    #[serde(default = "default_lookback_days")]
    pub lookback_days: u32,
    #[serde(default = "default_stream_profile")]
    pub stream_profile: StreamProfile,
    #[serde(default = "default_processing_lag_days")]
    pub processing_lag_days: u32,
    #[serde(default = "default_instagram_filter")]
    pub instagram_filter: bool,
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

fn default_instagram_filter() -> bool {
    true
}

pub struct DataSourceMetaInstagramAdsPlugin {
    config: DataSourceMetaInstagramAdsPluginConfig,
    http: RetryableHttpClient,
    static_auth: Option<StaticBearerAuth>,
    oauth: Option<OAuth2RefreshTokenAuth>,
    ad_account_id: String,
}

impl DataSourceMetaInstagramAdsPlugin {
    pub fn new(config: DataSourceMetaInstagramAdsPluginConfig) -> Result<Self, std::io::Error> {
        config.validate()?;
        let ad_account_id = normalize_ad_account_id(&config.ad_account_id);
        let (static_auth, oauth) = Self::build_auth(&config)?;
        let http = RetryableHttpClient::new(RetryConfig::default());
        Ok(Self {
            config,
            http,
            static_auth,
            oauth,
            ad_account_id,
        })
    }

    fn build_auth(
        config: &DataSourceMetaInstagramAdsPluginConfig,
    ) -> Result<(Option<StaticBearerAuth>, Option<OAuth2RefreshTokenAuth>), std::io::Error> {
        if let Some(token) = config
            .access_token
            .as_deref()
            .filter(|t| !t.trim().is_empty())
        {
            return Ok((Some(StaticBearerAuth::new(token.trim())), None));
        }
        if std::env::var("SKIPPR_META_INSTAGRAM_ADS_FIXTURE_DIR")
            .map(|d| !d.trim().is_empty())
            .unwrap_or(false)
        {
            return Ok((Some(StaticBearerAuth::new("fixture")), None));
        }
        if let Ok(token) = std::env::var("META_INSTAGRAM_ADS_ACCESS_TOKEN") {
            if !token.trim().is_empty() {
                return Ok((Some(StaticBearerAuth::new(token.trim())), None));
            }
        }
        let token_url = config.oauth_token_url.as_deref();
        let client_id = config.oauth_client_id.as_deref();
        let client_secret = config.oauth_client_secret.as_deref();
        let refresh_token = config.oauth_refresh_token.as_deref();
        if let (Some(token_url), Some(client_id), Some(client_secret), Some(refresh_token)) =
            (token_url, client_id, client_secret, refresh_token)
        {
            if token_url.is_empty()
                || client_id.is_empty()
                || client_secret.is_empty()
                || refresh_token.is_empty()
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Meta Instagram Ads OAuth fields must all be non-empty when configured",
                ));
            }
            return Ok((
                None,
                Some(OAuth2RefreshTokenAuth::new(
                    token_url,
                    client_id,
                    client_secret,
                    refresh_token,
                )),
            ));
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Meta Instagram Ads requires access_token, META_INSTAGRAM_ADS_ACCESS_TOKEN, or \
             oauth_token_url + oauth_client_id + oauth_client_secret + oauth_refresh_token",
        ))
    }

    async fn access_token(&self) -> Result<String, std::io::Error> {
        if let Some(auth) = &self.static_auth {
            return auth
                .authorization_header()
                .map(|h| strip_bearer_token(&h))
                .map_err(std::io::Error::other);
        }
        if let Some(oauth) = &self.oauth {
            return oauth.refresh().await.map_err(std::io::Error::other);
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Meta Instagram Ads auth is not configured",
        ))
    }

    fn api_client(&self) -> MetaInsightsApiClient {
        MetaInsightsApiClient::new(
            self.http.clone(),
            self.ad_account_id.clone(),
            api_version_or_default(self.config.api_version.as_deref()),
            self.config.instagram_filter,
        )
    }

    fn selected_streams(&self) -> Vec<&'static MetaStreamDef> {
        resolve_streams(self.config.stream_profile, self.config.streams.clone())
    }

    fn streams_for_run(&self, discover: bool) -> Vec<&'static MetaStreamDef> {
        if discover {
            resolve_streams(StreamProfile::Minimal, None)
        } else {
            self.selected_streams()
        }
    }

    fn namespace_contract(stream: &MetaStreamDef) -> SourceNamespaceContract {
        let mut primary_key = vec![
            FieldPath::single("ad_account_id"),
            FieldPath::single("date"),
        ];
        match stream.level {
            InsightsLevel::Account => {}
            InsightsLevel::Campaign => {
                primary_key.push(FieldPath::single("campaign_id"));
            }
            InsightsLevel::Adset => {
                primary_key.push(FieldPath::single("campaign_id"));
                primary_key.push(FieldPath::single("adset_id"));
            }
            InsightsLevel::Ad => {
                primary_key.push(FieldPath::single("campaign_id"));
                primary_key.push(FieldPath::single("adset_id"));
                primary_key.push(FieldPath::single("ad_id"));
            }
        }
        if stream.placement_breakdown {
            primary_key.push(FieldPath::single("publisher_platform"));
            primary_key.push(FieldPath::single("platform_position"));
        }
        SourceNamespaceContract {
            namespace: stream.namespace.to_string(),
            primary_key,
            cursor: Some(FieldPath::single("date")),
            partition_key: vec![FieldPath::single("date")],
            write_policy: WritePolicy::ReplacePartition,
            refresh_window: None,
            description: "Meta Instagram Ads mutable daily insights".into(),
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
        format!("meta_ig_ads:{}:{}", self.ad_account_id, namespace)
    }

    fn load_last_completed(ctx: &dyn SourceSyncContext, key: &str) -> Option<NaiveDate> {
        let Some(cp) = load_checkpoint_payload::<MetaNamespaceCheckpoint>(ctx, key) else {
            return None;
        };
        match Self::parse_date(&cp.last_completed_date) {
            Ok(date) => Some(date),
            Err(err) => {
                tracing::warn!("Meta Instagram Ads ignoring corrupt checkpoint for {key}: {err}");
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
            &MetaNamespaceCheckpoint {
                last_completed_date: date.format("%Y-%m-%d").to_string(),
            },
        )
        .map_err(|e| std::io::Error::other(e.to_string()))?;
        ctx.store_checkpoint(key, &envelope)
            .map_err(std::io::Error::other)
    }

    fn submit_rows_for_date(
        &self,
        ctx: &dyn SourceSyncContext,
        stream: &MetaStreamDef,
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
                source_uri: format!("meta-instagram-ads://act_{}/insights", self.ad_account_id),
                namespace: Some(stream.namespace.to_string()),
                cdc_rows: None,
            }],
        )?;
        Ok(())
    }

    async fn sync_stream_date(
        &self,
        api: &MetaInsightsApiClient,
        stream: &MetaStreamDef,
        date: NaiveDate,
        access_token: &str,
    ) -> Result<Vec<serde_json::Value>, std::io::Error> {
        let body = api
            .fetch_insights_all_pages(stream, date, access_token)
            .await?;
        Ok(parse_insight_rows(
            &body,
            stream,
            &self.ad_account_id,
            self.config.instagram_filter,
        ))
    }
}

fn strip_bearer_token(header: &str) -> String {
    header
        .trim()
        .strip_prefix("Bearer ")
        .or_else(|| header.trim().strip_prefix("bearer "))
        .unwrap_or(header.trim())
        .to_string()
}

impl DataSourceMetaInstagramAdsPluginConfig {
    pub fn validate(&self) -> Result<(), std::io::Error> {
        if normalize_ad_account_id(&self.ad_account_id).is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "ad_account_id is required",
            ));
        }
        Ok(())
    }
}

pub(crate) fn parse_insight_rows(
    body: &serde_json::Value,
    stream: &MetaStreamDef,
    ad_account_id: &str,
    instagram_filter: bool,
) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for row in rows_from_insights_body(body) {
        let Some(obj) = row.as_object() else {
            continue;
        };
        if instagram_filter {
            if let Some(platform) = obj.get("publisher_platform").and_then(|v| v.as_str()) {
                if !platform.eq_ignore_ascii_case("instagram") {
                    continue;
                }
            }
        }
        let mut record = serde_json::Map::new();
        record.insert(
            "ad_account_id".into(),
            serde_json::Value::String(ad_account_id.to_string()),
        );
        let date = obj
            .get("date_start")
            .or_else(|| obj.get("date_stop"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if !date.is_empty() {
            record.insert("date".into(), serde_json::Value::String(date));
        }
        copy_grain_ids(stream, obj, &mut record);
        for (key, value) in obj {
            if key == "date_start" || key == "date_stop" {
                continue;
            }
            if record.contains_key(key) {
                continue;
            }
            record.insert(key.clone(), value.clone());
        }
        out.push(serde_json::Value::Object(record));
    }
    out
}

fn copy_grain_ids(
    stream: &MetaStreamDef,
    source: &serde_json::Map<String, serde_json::Value>,
    record: &mut serde_json::Map<String, serde_json::Value>,
) {
    let mut copy_id = |field: &str, target: &str| {
        if let Some(value) = source.get(field) {
            record.insert(target.into(), normalize_id_value(value));
        }
    };
    match stream.level {
        InsightsLevel::Account => {}
        InsightsLevel::Campaign => {
            copy_id("campaign_id", "campaign_id");
        }
        InsightsLevel::Adset => {
            copy_id("campaign_id", "campaign_id");
            copy_id("adset_id", "adset_id");
        }
        InsightsLevel::Ad => {
            copy_id("campaign_id", "campaign_id");
            copy_id("adset_id", "adset_id");
            copy_id("ad_id", "ad_id");
        }
    }
    if stream.placement_breakdown {
        if let Some(value) = source.get("publisher_platform") {
            record.insert("publisher_platform".into(), value.clone());
        }
        if let Some(value) = source.get("platform_position") {
            record.insert("platform_position".into(), value.clone());
        }
    }
}

fn normalize_id_value(value: &serde_json::Value) -> serde_json::Value {
    if value.is_string() || value.is_number() {
        return value.clone();
    }
    if let Some(s) = value.as_str() {
        return serde_json::Value::String(s.to_string());
    }
    value.clone()
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
impl DataSource for DataSourceMetaInstagramAdsPlugin {
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
                .expect("invalid Meta Instagram Ads namespace contract configuration");
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
                "Meta Instagram Ads discover: sampling recent days only; full historical sync runs on skippr sync"
            );
            sample_start
        } else {
            configured_start
        };
        if end_date < start_date {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Meta Instagram Ads end date {end_date} is before start date {start_date}"),
            ));
        }

        let streams = self.streams_for_run(discover);
        if discover {
            info!(
                stream_count = streams.len(),
                stream_profile = "minimal",
                "Meta Instagram Ads discover: sampling minimal streams and recent days; contracts reflect configured profile"
            );
        }

        let api = self.api_client();
        let access_token = self.access_token().await?;
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
                    .sync_stream_date(&api, stream, date, &access_token)
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
    use crate::streams::{streams_for_profile, StreamProfile, CURATED_STREAMS, FULL_STREAM_COUNT};
    use skippr_plugin_shared_api_source::DateWindow;

    use std::sync::Mutex;

    fn env_test_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn test_config() -> DataSourceMetaInstagramAdsPluginConfig {
        DataSourceMetaInstagramAdsPluginConfig {
            ad_account_id: "act_123456789".into(),
            access_token: Some("token".into()),
            oauth_token_url: None,
            oauth_client_id: None,
            oauth_client_secret: None,
            oauth_refresh_token: None,
            api_version: None,
            start_date: "2024-01-01".into(),
            end_date: None,
            lookback_days: 3,
            stream_profile: StreamProfile::Full,
            processing_lag_days: 1,
            instagram_filter: true,
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
        let plugin = DataSourceMetaInstagramAdsPlugin::new(test_config()).unwrap();
        let discover_streams = plugin.streams_for_run(true);
        assert_eq!(discover_streams.len(), 1);
        assert_eq!(plugin.streams_for_run(false).len(), FULL_STREAM_COUNT);
    }

    #[test]
    fn namespace_contracts_use_replace_partition() {
        let plugin = DataSourceMetaInstagramAdsPlugin::new(test_config()).unwrap();
        let contracts = plugin.source_namespace_contracts();
        assert_eq!(contracts.len(), FULL_STREAM_COUNT);
        assert!(contracts
            .iter()
            .all(|c| c.write_policy == WritePolicy::ReplacePartition));
    }

    #[test]
    fn profile_stream_counts() {
        assert_eq!(streams_for_profile(StreamProfile::Minimal).len(), 1);
        assert_eq!(streams_for_profile(StreamProfile::Standard).len(), 3);
        assert_eq!(streams_for_profile(StreamProfile::Full).len(), 5);
    }

    #[test]
    fn parses_account_insights_fixture() {
        let fixture_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        let body: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(format!("{fixture_dir}/account_insights.json")).unwrap(),
        )
        .unwrap();
        let stream = streams_for_profile(StreamProfile::Minimal)[0];
        let rows = parse_insight_rows(&body, stream, "123456789", true);
        assert!(!rows.is_empty());
        assert_eq!(rows[0]["ad_account_id"], "123456789");
        assert_eq!(rows[0]["date"], "2024-01-01");
    }

    #[test]
    fn parses_campaign_adset_ad_and_placement_fixtures() {
        let fixture_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        for (file, namespace) in [
            (
                "campaign_insights.json",
                "meta_instagram_ads.campaign_daily",
            ),
            ("adset_insights.json", "meta_instagram_ads.adset_daily"),
            ("ad_insights.json", "meta_instagram_ads.ad_daily"),
            (
                "campaign_placement_insights.json",
                "meta_instagram_ads.campaign_placement_daily",
            ),
        ] {
            let body: serde_json::Value = serde_json::from_str(
                &std::fs::read_to_string(format!("{fixture_dir}/{file}")).unwrap(),
            )
            .unwrap();
            let stream = CURATED_STREAMS
                .iter()
                .find(|s| s.namespace == namespace)
                .unwrap();
            let rows = parse_insight_rows(&body, stream, "123456789", true);
            assert!(!rows.is_empty(), "expected rows in {file}");
            assert_eq!(rows[0]["ad_account_id"], "123456789");
            assert_eq!(rows[0]["date"], "2024-01-01");
        }
    }

    #[test]
    fn placement_contract_primary_key_includes_platform_fields() {
        let stream = streams_for_profile(StreamProfile::Full)
            .into_iter()
            .find(|s| s.placement_breakdown)
            .unwrap();
        let contract = DataSourceMetaInstagramAdsPlugin::namespace_contract(stream);
        let pk: Vec<String> = contract.primary_key.iter().map(|f| f.dotted()).collect();
        assert!(pk.contains(&"publisher_platform".to_string()));
        assert!(pk.contains(&"platform_position".to_string()));
    }

    #[test]
    fn loads_fixture_dir_for_account_rows() {
        let _lock = env_test_lock();
        let fixture_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        std::env::set_var("SKIPPR_META_INSTAGRAM_ADS_FIXTURE_DIR", fixture_dir);
        let plugin = DataSourceMetaInstagramAdsPlugin::new(test_config()).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let api = plugin.api_client();
        let stream = streams_for_profile(StreamProfile::Minimal)[0];
        let date = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
        let rows = rt
            .block_on(async {
                let token = plugin.access_token().await.unwrap();
                plugin.sync_stream_date(&api, stream, date, &token).await
            })
            .expect("fixture sync");
        assert!(!rows.is_empty());
        std::env::remove_var("SKIPPR_META_INSTAGRAM_ADS_FIXTURE_DIR");
    }

    #[test]
    fn normalize_ad_account_id_strips_act_prefix() {
        assert_eq!(normalize_ad_account_id("act_999"), "999");
        assert_eq!(normalize_ad_account_id("888"), "888");
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
    fn meta_checkpoint_roundtrip_envelope() {
        let cp = MetaNamespaceCheckpoint {
            last_completed_date: "2024-03-01".into(),
        };
        let envelope = CheckpointEnvelope::from_payload(
            CheckpointAuthority::AdvisoryHint,
            CheckpointKind::SourceResume,
            CHECKPOINT_PAYLOAD_VERSION,
            &cp,
        )
        .expect("checkpoint envelope");
        let decoded: MetaNamespaceCheckpoint = envelope.into_payload().expect("payload");
        assert_eq!(decoded.last_completed_date, "2024-03-01");
    }

    #[test]
    fn config_defaults_and_validation_succeed() {
        let cfg: DataSourceMetaInstagramAdsPluginConfig =
            serde_json::from_value(serde_json::json!({
                "ad_account_id": "act_123",
                "access_token": "token",
                "start_date": "2024-01-01"
            }))
            .expect("deserialize");
        assert_eq!(cfg.stream_profile, StreamProfile::Full);
        assert!(cfg.instagram_filter);
        cfg.validate().expect("valid ad_account_id");
        DataSourceMetaInstagramAdsPlugin::new(cfg).expect("plugin init");
    }

    #[test]
    fn namespace_contracts_use_mutable_report_semantics() {
        let plugin = DataSourceMetaInstagramAdsPlugin::new(test_config()).unwrap();
        let contracts = plugin.source_namespace_contracts();
        assert!(contracts.iter().all(|c| {
            c.semantics == Some(SourceSemantics::MutableReport)
                && c.partition_key.iter().any(|f| f.dotted() == "date")
        }));
    }

    #[test]
    fn contract_primary_key_includes_level_dimensions() {
        for stream in CURATED_STREAMS {
            let contract = DataSourceMetaInstagramAdsPlugin::namespace_contract(stream);
            let pk: Vec<String> = contract.primary_key.iter().map(|f| f.dotted()).collect();
            assert!(pk.contains(&"ad_account_id".to_string()));
            assert!(pk.contains(&"date".to_string()));
            match stream.level {
                InsightsLevel::Account => {}
                InsightsLevel::Campaign => {
                    assert!(
                        pk.contains(&"campaign_id".to_string()),
                        "{}",
                        stream.namespace
                    );
                }
                InsightsLevel::Adset => {
                    assert!(pk.contains(&"adset_id".to_string()), "{}", stream.namespace);
                }
                InsightsLevel::Ad => {
                    assert!(pk.contains(&"ad_id".to_string()), "{}", stream.namespace);
                }
            }
        }
    }

    #[test]
    fn static_bearer_auth_selected_when_access_token_set() {
        let plugin = DataSourceMetaInstagramAdsPlugin::new(test_config()).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let token = rt.block_on(plugin.access_token()).unwrap();
        assert_eq!(token, "token");
    }

    #[test]
    fn oauth_refresh_auth_accepted_when_fully_configured() {
        let cfg = DataSourceMetaInstagramAdsPluginConfig {
            ad_account_id: "act_123".into(),
            access_token: None,
            oauth_token_url: Some("https://graph.facebook.com/v21.0/oauth/access_token".into()),
            oauth_client_id: Some("client".into()),
            oauth_client_secret: Some("secret".into()),
            oauth_refresh_token: Some("refresh".into()),
            api_version: None,
            start_date: "2024-01-01".into(),
            end_date: None,
            lookback_days: 3,
            stream_profile: StreamProfile::Minimal,
            processing_lag_days: 1,
            instagram_filter: true,
            streams: None,
        };
        DataSourceMetaInstagramAdsPlugin::new(cfg).expect("oauth config accepted");
    }

    #[test]
    fn missing_credentials_returns_clear_error() {
        let _lock = env_test_lock();
        let cfg = DataSourceMetaInstagramAdsPluginConfig {
            ad_account_id: "act_123".into(),
            access_token: None,
            oauth_token_url: None,
            oauth_client_id: None,
            oauth_client_secret: None,
            oauth_refresh_token: None,
            api_version: None,
            start_date: "2024-01-01".into(),
            end_date: None,
            lookback_days: 3,
            stream_profile: StreamProfile::Minimal,
            processing_lag_days: 1,
            instagram_filter: true,
            streams: None,
        };
        std::env::remove_var("SKIPPR_META_INSTAGRAM_ADS_FIXTURE_DIR");
        std::env::remove_var("META_INSTAGRAM_ADS_ACCESS_TOKEN");
        let err = match DataSourceMetaInstagramAdsPlugin::new(cfg) {
            Err(err) => err,
            Ok(_) => panic!("expected missing credentials error"),
        };
        assert!(err.to_string().contains("access_token"));
    }

    #[test]
    fn empty_ad_account_id_validation_fails() {
        let mut cfg = test_config();
        cfg.ad_account_id = "  ".into();
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("ad_account_id is required"));
    }

    #[test]
    fn invalid_start_date_rejected() {
        let err = DataSourceMetaInstagramAdsPlugin::parse_date("not-a-date").unwrap_err();
        assert!(err.to_string().contains("invalid date"));
    }

    #[test]
    fn end_date_before_start_date_rejected_on_sync() {
        let _lock = env_test_lock();
        let fixture_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        std::env::set_var("SKIPPR_META_INSTAGRAM_ADS_FIXTURE_DIR", fixture_dir);
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);

        let mut cfg = test_config();
        cfg.start_date = "2024-12-01".into();
        cfg.end_date = Some("2024-01-01".into());
        let mut plugin = DataSourceMetaInstagramAdsPlugin::new(cfg).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        let err = rt.block_on(plugin.sync(ctx)).expect_err("end before start");
        assert!(err.to_string().contains("before start date"));

        std::env::remove_var("SKIPPR_META_INSTAGRAM_ADS_FIXTURE_DIR");
    }

    #[test]
    fn parse_insights_prefers_date_start_when_mismatch() {
        let stream = streams_for_profile(StreamProfile::Minimal)[0];
        let body = serde_json::json!({
            "data": [{
                "date_start": "2024-01-01",
                "date_stop": "2024-01-02",
                "impressions": "10"
            }]
        });
        let rows = parse_insight_rows(&body, stream, "123", true);
        assert_eq!(rows[0]["date"], "2024-01-01");
    }

    #[test]
    fn parse_empty_insights_data_returns_no_rows() {
        let stream = streams_for_profile(StreamProfile::Minimal)[0];
        let rows = parse_insight_rows(&serde_json::json!({"data": []}), stream, "123", true);
        assert!(rows.is_empty());
    }

    #[test]
    fn parse_malformed_insights_body_returns_no_rows() {
        let stream = streams_for_profile(StreamProfile::Minimal)[0];
        assert!(parse_insight_rows(&serde_json::json!({}), stream, "123", true).is_empty());
        assert!(
            parse_insight_rows(&serde_json::json!({"data": "bad"}), stream, "123", true).is_empty()
        );
    }

    #[test]
    fn instagram_filter_drops_non_instagram_publisher_platform() {
        let stream = CURATED_STREAMS
            .iter()
            .find(|s| s.placement_breakdown)
            .unwrap();
        let body = serde_json::json!({
            "data": [{
                "date_start": "2024-01-01",
                "date_stop": "2024-01-01",
                "campaign_id": "1",
                "publisher_platform": "facebook",
                "platform_position": "feed",
                "impressions": "5"
            }]
        });
        assert!(parse_insight_rows(&body, stream, "123", true).is_empty());
        assert_eq!(parse_insight_rows(&body, stream, "123", false).len(), 1);
    }

    #[test]
    fn non_instagram_rows_parsed_when_filter_disabled() {
        let stream = CURATED_STREAMS
            .iter()
            .find(|s| s.placement_breakdown)
            .unwrap();
        let body = serde_json::json!({
            "data": [{
                "date_start": "2024-01-01",
                "date_stop": "2024-01-01",
                "campaign_id": "1",
                "publisher_platform": "facebook",
                "platform_position": "feed",
                "impressions": "5"
            }]
        });
        let rows = parse_insight_rows(&body, stream, "123", false);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["publisher_platform"], "facebook");
    }

    #[test]
    fn sync_mode_stores_checkpoints() {
        let _lock = env_test_lock();
        let fixture_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        std::env::set_var("SKIPPR_META_INSTAGRAM_ADS_FIXTURE_DIR", fixture_dir);
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);

        let mut cfg = test_config();
        cfg.stream_profile = StreamProfile::Minimal;
        cfg.access_token = None;
        cfg.start_date = "2024-01-01".into();
        cfg.end_date = Some("2024-01-01".into());
        let mut plugin = DataSourceMetaInstagramAdsPlugin::new(cfg).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        rt.block_on(plugin.sync(ctx.clone())).expect("sync");
        assert!(
            !ctx.checkpoint_stores.lock().unwrap().is_empty(),
            "sync should persist checkpoints per namespace/day"
        );

        std::env::remove_var("SKIPPR_META_INSTAGRAM_ADS_FIXTURE_DIR");
    }

    #[test]
    fn oauth_partial_empty_fields_rejected() {
        let _lock = env_test_lock();
        std::env::remove_var("SKIPPR_META_INSTAGRAM_ADS_FIXTURE_DIR");
        for key in [
            "META_ADS_ACCESS_TOKEN",
            "META_INSTAGRAM_ADS_ACCESS_TOKEN",
            "LINKEDIN_ADS_ACCESS_TOKEN",
            "X_ADS_ACCESS_TOKEN",
            "ADROLL_ADS_ACCESS_TOKEN",
        ] {
            std::env::remove_var(key);
        }
        let cfg = DataSourceMetaInstagramAdsPluginConfig {
            ad_account_id: "act_123".into(),
            access_token: None,
            oauth_token_url: Some("https://graph.facebook.com/v21.0/oauth/access_token".into()),
            oauth_client_id: Some("client".into()),
            oauth_client_secret: Some("".into()),
            oauth_refresh_token: Some("refresh".into()),
            api_version: None,
            start_date: "2024-01-01".into(),
            end_date: None,
            lookback_days: 3,
            stream_profile: StreamProfile::Minimal,
            processing_lag_days: 1,
            instagram_filter: true,
            streams: None,
        };
        let err = match DataSourceMetaInstagramAdsPlugin::new(cfg) {
            Ok(_) => panic!("expected OAuth config error"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("OAuth"));
    }

    #[test]
    fn discover_sync_does_not_store_checkpoints() {
        let _lock = env_test_lock();
        let fixture_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        std::env::set_var("SKIPPR_META_INSTAGRAM_ADS_FIXTURE_DIR", fixture_dir);
        std::env::set_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV, "discover");

        let mut cfg = test_config();
        cfg.stream_profile = StreamProfile::Minimal;
        cfg.start_date = "2024-01-01".into();
        cfg.end_date = Some("2024-01-01".into());
        let mut plugin = DataSourceMetaInstagramAdsPlugin::new(cfg).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ctx = Arc::new(RecordingSyncContext::default());
        rt.block_on(plugin.sync(ctx.clone()))
            .expect("discover sync");
        assert!(
            ctx.checkpoint_stores.lock().unwrap().is_empty(),
            "discover must not persist checkpoints"
        );

        std::env::remove_var("SKIPPR_META_INSTAGRAM_ADS_FIXTURE_DIR");
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
