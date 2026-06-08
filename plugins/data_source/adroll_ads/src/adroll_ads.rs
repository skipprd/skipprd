use crate::adroll_ads_api::{parse_rows, AdRollAdsApiClient};
use crate::streams::{resolve_streams, AdRollStreamDef, AdRollStreamKind, StreamProfile};
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
use std::sync::Arc;
const CHECKPOINT_PAYLOAD_VERSION: u32 = 1;
const DISCOVER_SAMPLE_DAYS: u32 = 3;
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct AdRollCheckpoint {
    last_completed_date: String,
}
#[derive(Debug, Clone, Deserialize)]
pub struct DataSourceAdRollAdsPluginConfig {
    pub advertiser_id: String,
    #[serde(default)]
    pub access_token: Option<String>,
    #[serde(default)]
    pub personal_access_token: Option<String>,
    #[serde(default)]
    pub oauth_token_url: Option<String>,
    #[serde(default)]
    pub oauth_client_id: Option<String>,
    #[serde(default)]
    pub oauth_client_secret: Option<String>,
    #[serde(default)]
    pub oauth_refresh_token: Option<String>,
    #[serde(default)]
    pub api_base_url: Option<String>,
    #[serde(default)]
    pub reporting_base_url: Option<String>,
    pub start_date: String,
    #[serde(default)]
    pub end_date: Option<String>,
    #[serde(default = "default_lookback_days")]
    pub lookback_days: u32,
    #[serde(default = "default_stream_profile")]
    pub stream_profile: StreamProfile,
    #[serde(default = "default_processing_lag_days")]
    pub processing_lag_days: u32,
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
pub struct DataSourceAdRollAdsPlugin {
    config: DataSourceAdRollAdsPluginConfig,
    http: RetryableHttpClient,
    oauth: Option<OAuth2RefreshTokenAuth>,
    advertiser_id: String,
}
impl DataSourceAdRollAdsPlugin {
    pub fn new(config: DataSourceAdRollAdsPluginConfig) -> Result<Self, std::io::Error> {
        config.validate()?;
        let oauth = build_oauth(&config);
        let advertiser_id = config.advertiser_id.trim().to_string();
        if config
            .access_token
            .as_deref()
            .or(config.personal_access_token.as_deref())
            .unwrap_or_default()
            .trim()
            .is_empty()
            && oauth.is_none()
            && std::env::var("ADROLL_ADS_ACCESS_TOKEN")
                .ok()
                .as_deref()
                .unwrap_or_default()
                .trim()
                .is_empty()
            && !std::env::var("SKIPPR_ADROLL_ADS_FIXTURE_DIR")
                .map(|d| !d.trim().is_empty())
                .unwrap_or(false)
        {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "AdRoll Ads requires access_token, personal_access_token, ADROLL_ADS_ACCESS_TOKEN, or OAuth refresh credentials"));
        }
        Ok(Self {
            config,
            http: RetryableHttpClient::new(RetryConfig::default()),
            oauth,
            advertiser_id,
        })
    }
    async fn access_token(&self) -> Result<String, std::io::Error> {
        if let Some(token) = self
            .config
            .access_token
            .as_deref()
            .or(self.config.personal_access_token.as_deref())
            .filter(|t| !t.trim().is_empty())
        {
            return Ok(token.trim().to_string());
        }
        if let Ok(token) = std::env::var("ADROLL_ADS_ACCESS_TOKEN") {
            if !token.trim().is_empty() {
                return Ok(token.trim().to_string());
            }
        }
        if std::env::var("SKIPPR_ADROLL_ADS_FIXTURE_DIR")
            .map(|d| !d.trim().is_empty())
            .unwrap_or(false)
        {
            return Ok("fixture".into());
        }
        if let Some(oauth) = &self.oauth {
            return oauth.refresh().await.map_err(std::io::Error::other);
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "AdRoll Ads auth is not configured",
        ))
    }
    fn selected_streams(&self) -> Vec<&'static AdRollStreamDef> {
        resolve_streams(self.config.stream_profile, self.config.streams.clone())
    }
    fn streams_for_run(&self, discover: bool) -> Vec<&'static AdRollStreamDef> {
        if discover {
            resolve_streams(StreamProfile::Minimal, None)
        } else {
            self.selected_streams()
        }
    }
    fn api_client(&self, token: String) -> AdRollAdsApiClient {
        let api_base_url = self
            .config
            .api_base_url
            .clone()
            .or_else(|| std::env::var("ADROLL_ADS_API_BASE").ok());
        let reporting_url = self
            .config
            .reporting_base_url
            .clone()
            .or_else(|| std::env::var("ADROLL_ADS_REPORTING_URL").ok());
        AdRollAdsApiClient::new(
            self.http.clone(),
            self.advertiser_id.clone(),
            token,
            api_base_url,
            reporting_url,
        )
    }
    fn namespace_contract(s: &AdRollStreamDef) -> SourceNamespaceContract {
        SourceNamespaceContract {
            namespace: s.namespace.to_string(),
            primary_key: s
                .primary_key
                .iter()
                .map(|k| FieldPath::single(*k))
                .collect(),
            cursor: s.cursor.map(FieldPath::single),
            partition_key: s
                .cursor
                .map(|k| vec![FieldPath::single(k)])
                .unwrap_or_default(),
            write_policy: if s.cursor.is_some() {
                WritePolicy::ReplacePartition
            } else {
                WritePolicy::Append
            },
            refresh_window: None,
            description: "AdRoll / NextRoll Ads API source contract".into(),
            semantics: Some(if s.cursor.is_some() {
                SourceSemantics::MutableReport
            } else {
                SourceSemantics::EntityState
            }),
        }
    }
    fn parse_date(v: &str) -> Result<NaiveDate, std::io::Error> {
        NaiveDate::parse_from_str(v, "%Y-%m-%d").map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid date '{v}': {e}"),
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
        Ok(configured
            .min(Utc::now().date_naive() - Duration::days(self.config.processing_lag_days as i64)))
    }
    fn checkpoint_key(&self, ns: &str) -> String {
        format!("adroll_ads:{}:{}", self.advertiser_id, ns)
    }
    fn load_last_completed(ctx: &dyn SourceSyncContext, key: &str) -> Option<NaiveDate> {
        load_checkpoint_payload::<AdRollCheckpoint>(ctx, key)
            .and_then(|cp| Self::parse_date(&cp.last_completed_date).ok())
    }
    fn store_last_completed(
        ctx: &dyn SourceSyncContext,
        key: &str,
        date: NaiveDate,
    ) -> Result<(), std::io::Error> {
        let e = CheckpointEnvelope::from_payload(
            CheckpointAuthority::AdvisoryHint,
            CheckpointKind::SourceResume,
            CHECKPOINT_PAYLOAD_VERSION,
            &AdRollCheckpoint {
                last_completed_date: date.format("%Y-%m-%d").to_string(),
            },
        )
        .map_err(|e| std::io::Error::other(e.to_string()))?;
        ctx.store_checkpoint(key, &e).map_err(std::io::Error::other)
    }
    fn submit(
        &self,
        ctx: &dyn SourceSyncContext,
        s: &AdRollStreamDef,
        offset: String,
        rows: Vec<serde_json::Value>,
    ) -> Result<(), std::io::Error> {
        if rows.is_empty() {
            return Ok(());
        }
        let payload = rows
            .into_iter()
            .map(|r| serde_json::to_string(&r))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| std::io::Error::other(e.to_string()))?
            .join("\n");
        let bytes = payload.len();
        submit_payload_batches(
            ctx,
            vec![IngestBatch {
                offset_key: OffsetKey::new(s.namespace, offset),
                data: payload,
                bytes,
                offset_pos: None,
                source_uri: format!("adroll-ads://{}", self.advertiser_id),
                namespace: Some(s.namespace.to_string()),
                cdc_rows: None,
            }],
        )?;
        Ok(())
    }
}
fn build_oauth(c: &DataSourceAdRollAdsPluginConfig) -> Option<OAuth2RefreshTokenAuth> {
    let token_url = c.oauth_token_url.as_deref()?.trim();
    let client_id = c.oauth_client_id.as_deref()?.trim();
    let client_secret = c.oauth_client_secret.as_deref()?.trim();
    let refresh_token = c.oauth_refresh_token.as_deref()?.trim();
    if token_url.is_empty()
        || client_id.is_empty()
        || client_secret.is_empty()
        || refresh_token.is_empty()
    {
        return None;
    }
    Some(OAuth2RefreshTokenAuth::new(
        token_url,
        client_id,
        client_secret,
        refresh_token,
    ))
}
impl DataSourceAdRollAdsPluginConfig {
    pub fn validate(&self) -> Result<(), std::io::Error> {
        if self.advertiser_id.trim().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "advertiser_id is required",
            ));
        }
        DataSourceAdRollAdsPlugin::parse_date(&self.start_date)?;
        let oauth_fields = [
            self.oauth_token_url.as_deref(),
            self.oauth_client_id.as_deref(),
            self.oauth_client_secret.as_deref(),
            self.oauth_refresh_token.as_deref(),
        ];
        let oauth_present = oauth_fields
            .iter()
            .any(|v| v.unwrap_or("").trim().len() > 0);
        let oauth_complete = oauth_fields
            .iter()
            .all(|v| v.unwrap_or("").trim().len() > 0);
        if oauth_present && !oauth_complete {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "AdRoll Ads OAuth refresh requires oauth_token_url, oauth_client_id, oauth_client_secret, and oauth_refresh_token",
            ));
        }
        Ok(())
    }
}
fn runtime_is_discover_mode() -> bool {
    std::env::var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV)
        .map(|m| m.eq_ignore_ascii_case("discover"))
        .unwrap_or(false)
}
fn discover_sample_date_window(end: NaiveDate) -> (NaiveDate, NaiveDate) {
    let span = DISCOVER_SAMPLE_DAYS.max(1);
    (end - Duration::days(i64::from(span - 1)), end)
}
#[async_trait]
impl DataSource for DataSourceAdRollAdsPlugin {
    fn execution_contract(&self) -> SourceExecutionContract {
        SourceExecutionContract::stream(SourceOnceContract::Finite)
    }
    fn source_namespace_contracts(&self) -> Vec<SourceNamespaceContract> {
        self.selected_streams()
            .iter()
            .map(|s| {
                let mut c = Self::namespace_contract(s);
                c.refresh_window = if s.cursor.is_some() {
                    Some(self.config.lookback_days)
                } else {
                    None
                };
                c
            })
            .collect()
    }
    async fn sync(&mut self, ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error> {
        let discover = runtime_is_discover_mode();
        let start = Self::parse_date(&self.config.start_date)?;
        let end = self.effective_end_date()?;
        let token = self.access_token().await?;
        let api = self.api_client(token);
        for stream in self.streams_for_run(discover) {
            match stream.kind {
                AdRollStreamKind::Reporting => {
                    let key = self.checkpoint_key(stream.namespace);
                    let last = if discover {
                        None
                    } else {
                        Self::load_last_completed(ctx.as_ref(), &key)
                    };
                    let end = if discover {
                        discover_sample_date_window(end).1
                    } else {
                        end
                    };
                    let window = DateWindowPlanner {
                        lookback_days: if discover {
                            0
                        } else {
                            self.config.lookback_days
                        },
                    }
                    .plan(start, last, end);
                    for date in DateWindowPlanner::dates_inclusive(&window) {
                        let body = api.fetch(stream, Some(date)).await?;
                        let rows = parse_rows(&body, stream, &self.advertiser_id, Some(date));
                        self.submit(
                            ctx.as_ref(),
                            stream,
                            date.format("%Y-%m-%d").to_string(),
                            rows,
                        )?;
                        if !discover {
                            Self::store_last_completed(ctx.as_ref(), &key, date)?;
                        }
                    }
                }
                _ => {
                    let body = api.fetch(stream, None).await?;
                    let rows = parse_rows(&body, stream, &self.advertiser_id, None);
                    self.submit(ctx.as_ref(), stream, "snapshot".into(), rows)?;
                }
            }
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepts_pat_fallback() {
        let cfg = DataSourceAdRollAdsPluginConfig {
            advertiser_id: "adv".into(),
            access_token: None,
            personal_access_token: Some("pat".into()),
            oauth_token_url: None,
            oauth_client_id: None,
            oauth_client_secret: None,
            oauth_refresh_token: None,
            api_base_url: None,
            reporting_base_url: None,
            start_date: "2024-01-01".into(),
            end_date: None,
            lookback_days: 3,
            stream_profile: StreamProfile::Full,
            processing_lag_days: 1,
            streams: None,
        };
        let plugin = DataSourceAdRollAdsPlugin::new(cfg).unwrap();
        assert!(plugin
            .source_namespace_contracts()
            .iter()
            .any(|c| c.namespace == "adroll_ads.reporting_daily"));
    }

    #[test]
    fn partial_oauth_refresh_config_is_rejected() {
        let mut cfg = DataSourceAdRollAdsPluginConfig {
            advertiser_id: "adv".into(),
            access_token: None,
            personal_access_token: Some("pat".into()),
            oauth_token_url: Some("https://oauth.example/token".into()),
            oauth_client_id: None,
            oauth_client_secret: None,
            oauth_refresh_token: None,
            api_base_url: None,
            reporting_base_url: None,
            start_date: "2024-01-01".into(),
            end_date: None,
            lookback_days: 3,
            stream_profile: StreamProfile::Full,
            processing_lag_days: 1,
            streams: None,
        };
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("OAuth refresh requires"));
        cfg.oauth_client_id = Some("client".into());
        cfg.oauth_client_secret = Some("secret".into());
        cfg.oauth_refresh_token = Some("refresh".into());
        cfg.validate().expect("complete oauth quartet");
    }
}
