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

use crate::linkedin_ads_api::{
    normalize_account_id, parse_rows, rest_version_or_default, LinkedInAdsApiClient,
};
use crate::streams::{resolve_streams, LinkedInStreamDef, LinkedInStreamKind, StreamProfile};

const CHECKPOINT_PAYLOAD_VERSION: u32 = 1;
const DISCOVER_SAMPLE_DAYS: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct LinkedInNamespaceCheckpoint {
    last_completed_date: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DataSourceLinkedInAdsPluginConfig {
    pub ad_account_id: String,
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
    pub rest_version: Option<String>,
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

pub struct DataSourceLinkedInAdsPlugin {
    config: DataSourceLinkedInAdsPluginConfig,
    http: RetryableHttpClient,
    static_auth: Option<StaticBearerAuth>,
    oauth: Option<OAuth2RefreshTokenAuth>,
    account_id: String,
}

impl DataSourceLinkedInAdsPlugin {
    pub fn new(config: DataSourceLinkedInAdsPluginConfig) -> Result<Self, std::io::Error> {
        config.validate()?;
        let account_id = normalize_account_id(&config.ad_account_id);
        let (static_auth, oauth) = build_auth(&config)?;
        Ok(Self {
            config,
            http: RetryableHttpClient::new(RetryConfig::default()),
            static_auth,
            oauth,
            account_id,
        })
    }
    async fn access_token(&self) -> Result<String, std::io::Error> {
        access_token(self.static_auth.as_ref(), self.oauth.as_ref()).await
    }
    fn selected_streams(&self) -> Vec<&'static LinkedInStreamDef> {
        resolve_streams(self.config.stream_profile, self.config.streams.clone())
    }
    fn streams_for_run(&self, discover: bool) -> Vec<&'static LinkedInStreamDef> {
        if discover {
            resolve_streams(StreamProfile::Minimal, None)
        } else {
            self.selected_streams()
        }
    }
    fn api_client(&self) -> LinkedInAdsApiClient {
        LinkedInAdsApiClient::new(
            self.http.clone(),
            self.account_id.clone(),
            rest_version_or_default(
                self.config
                    .rest_version
                    .as_deref()
                    .or(self.config.api_version.as_deref()),
            ),
        )
    }
    fn namespace_contract(stream: &LinkedInStreamDef) -> SourceNamespaceContract {
        SourceNamespaceContract {
            namespace: stream.namespace.to_string(),
            primary_key: stream
                .primary_key
                .iter()
                .map(|k| FieldPath::single(*k))
                .collect(),
            cursor: stream.cursor.map(FieldPath::single),
            partition_key: stream
                .cursor
                .map(|k| vec![FieldPath::single(k)])
                .unwrap_or_default(),
            write_policy: if stream.cursor.is_some() {
                WritePolicy::ReplacePartition
            } else {
                WritePolicy::Append
            },
            refresh_window: None,
            description: "LinkedIn Marketing API source contract".into(),
            semantics: Some(if stream.cursor.is_some() {
                SourceSemantics::MutableReport
            } else {
                SourceSemantics::EntityState
            }),
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
        Ok(configured
            .min(Utc::now().date_naive() - Duration::days(self.config.processing_lag_days as i64)))
    }
    fn checkpoint_key(&self, namespace: &str) -> String {
        format!("linkedin_ads:{}:{}", self.account_id, namespace)
    }
    fn load_last_completed(ctx: &dyn SourceSyncContext, key: &str) -> Option<NaiveDate> {
        load_checkpoint_payload::<LinkedInNamespaceCheckpoint>(ctx, key)
            .and_then(|cp| Self::parse_date(&cp.last_completed_date).ok())
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
            &LinkedInNamespaceCheckpoint {
                last_completed_date: date.format("%Y-%m-%d").to_string(),
            },
        )
        .map_err(|e| std::io::Error::other(e.to_string()))?;
        ctx.store_checkpoint(key, &envelope)
            .map_err(std::io::Error::other)
    }
    fn submit(
        &self,
        ctx: &dyn SourceSyncContext,
        stream: &LinkedInStreamDef,
        offset: String,
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
        submit_payload_batches(
            ctx,
            vec![IngestBatch {
                offset_key: OffsetKey::new(stream.namespace, offset),
                data: payload,
                bytes,
                offset_pos: None,
                source_uri: format!("linkedin-ads://{}", self.account_id),
                namespace: Some(stream.namespace.to_string()),
                cdc_rows: None,
            }],
        )?;
        Ok(())
    }
}

fn build_auth(
    config: &DataSourceLinkedInAdsPluginConfig,
) -> Result<(Option<StaticBearerAuth>, Option<OAuth2RefreshTokenAuth>), std::io::Error> {
    if let Some(token) = config
        .access_token
        .as_deref()
        .filter(|t| !t.trim().is_empty())
    {
        return Ok((Some(StaticBearerAuth::new(token.trim())), None));
    }
    if std::env::var("SKIPPR_LINKEDIN_ADS_FIXTURE_DIR")
        .map(|d| !d.trim().is_empty())
        .unwrap_or(false)
    {
        return Ok((Some(StaticBearerAuth::new("fixture")), None));
    }
    if let Ok(token) = std::env::var("LINKEDIN_ADS_ACCESS_TOKEN") {
        if !token.trim().is_empty() {
            return Ok((Some(StaticBearerAuth::new(token.trim())), None));
        }
    }
    let token_url = config.oauth_token_url.as_deref().map(str::trim);
    let client_id = config.oauth_client_id.as_deref().map(str::trim);
    let client_secret = config.oauth_client_secret.as_deref().map(str::trim);
    let refresh_token = config.oauth_refresh_token.as_deref().map(str::trim);
    if let (Some(url), Some(id), Some(secret), Some(refresh)) =
        (token_url, client_id, client_secret, refresh_token)
    {
        if url.is_empty() || id.is_empty() || secret.is_empty() || refresh.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "LinkedIn Ads OAuth refresh requires oauth_token_url, oauth_client_id, oauth_client_secret, and oauth_refresh_token",
            ));
        }
        return Ok((
            None,
            Some(OAuth2RefreshTokenAuth::new(url, id, secret, refresh)),
        ));
    }
    Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "LinkedIn Ads requires access_token, LINKEDIN_ADS_ACCESS_TOKEN, or OAuth refresh credentials"))
}
async fn access_token(
    static_auth: Option<&StaticBearerAuth>,
    oauth: Option<&OAuth2RefreshTokenAuth>,
) -> Result<String, std::io::Error> {
    if let Some(auth) = static_auth {
        return auth
            .authorization_header()
            .map(strip_bearer)
            .map_err(std::io::Error::other);
    }
    if let Ok(token) = std::env::var("LINKEDIN_ADS_ACCESS_TOKEN") {
        if !token.trim().is_empty() {
            return Ok(token.trim().to_string());
        }
    }
    if let Some(oauth) = oauth {
        return oauth.refresh().await.map_err(std::io::Error::other);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "LinkedIn Ads auth is not configured",
    ))
}
fn strip_bearer(header: String) -> String {
    header
        .trim()
        .strip_prefix("Bearer ")
        .unwrap_or(header.trim())
        .to_string()
}
fn runtime_is_discover_mode() -> bool {
    std::env::var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV)
        .map(|mode| mode.eq_ignore_ascii_case("discover"))
        .unwrap_or(false)
}
fn discover_sample_date_window(end_date: NaiveDate) -> (NaiveDate, NaiveDate) {
    let span = DISCOVER_SAMPLE_DAYS.max(1);
    (end_date - Duration::days(i64::from(span - 1)), end_date)
}

impl DataSourceLinkedInAdsPluginConfig {
    pub fn validate(&self) -> Result<(), std::io::Error> {
        if normalize_account_id(&self.ad_account_id).is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "ad_account_id is required",
            ));
        }
        DataSourceLinkedInAdsPlugin::parse_date(&self.start_date)?;
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
                "LinkedIn Ads OAuth refresh requires oauth_token_url, oauth_client_id, oauth_client_secret, and oauth_refresh_token",
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl DataSource for DataSourceLinkedInAdsPlugin {
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
        let start_date = Self::parse_date(&self.config.start_date)?;
        let end_date = self.effective_end_date()?;
        let token = self.access_token().await?;
        let api = self.api_client();
        for stream in self.streams_for_run(discover) {
            match stream.kind {
                LinkedInStreamKind::AdAnalytics => {
                    let key = self.checkpoint_key(stream.namespace);
                    let last = if discover {
                        None
                    } else {
                        Self::load_last_completed(ctx.as_ref(), &key)
                    };
                    let end = if discover {
                        discover_sample_date_window(end_date).1
                    } else {
                        end_date
                    };
                    let window = DateWindowPlanner {
                        lookback_days: if discover {
                            0
                        } else {
                            self.config.lookback_days
                        },
                    }
                    .plan(start_date, last, end);
                    for date in DateWindowPlanner::dates_inclusive(&window) {
                        let body = api.fetch(stream, Some(date), &token).await?;
                        let rows = parse_rows(&body, stream, &self.account_id, Some(date));
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
                    let body = api.fetch(stream, None, &token).await?;
                    let rows = parse_rows(&body, stream, &self.account_id, None);
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
    fn contracts_are_not_meta_clone() {
        let cfg = DataSourceLinkedInAdsPluginConfig {
            ad_account_id: "123".into(),
            access_token: Some("t".into()),
            oauth_token_url: None,
            oauth_client_id: None,
            oauth_client_secret: None,
            oauth_refresh_token: None,
            rest_version: None,
            api_version: None,
            start_date: "2024-01-01".into(),
            end_date: None,
            lookback_days: 3,
            stream_profile: StreamProfile::Full,
            processing_lag_days: 1,
            streams: None,
        };
        let plugin = DataSourceLinkedInAdsPlugin::new(cfg).unwrap();
        let namespaces: Vec<_> = plugin
            .source_namespace_contracts()
            .into_iter()
            .map(|c| c.namespace)
            .collect();
        assert!(namespaces.contains(&"linkedin_ads.creatives".to_string()));
        assert!(!namespaces.iter().any(|n| n.contains("adset")));
    }

    #[test]
    fn partial_oauth_refresh_config_is_rejected() {
        let mut cfg = DataSourceLinkedInAdsPluginConfig {
            ad_account_id: "urn:li:sponsoredAccount:123".into(),
            access_token: None,
            oauth_token_url: Some("https://www.linkedin.com/oauth/v2/accessToken".into()),
            oauth_client_id: None,
            oauth_client_secret: None,
            oauth_refresh_token: None,
            rest_version: None,
            api_version: None,
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
