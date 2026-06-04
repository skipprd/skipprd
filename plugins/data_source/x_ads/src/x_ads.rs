use crate::streams::{resolve_streams, StreamProfile, XAdsStreamDef, XAdsStreamKind};
use crate::x_ads_api::{normalize_account_id, parse_rows, XAdsApiClient, XOAuth1Credentials};
use async_trait::async_trait;
use chrono::{Duration, NaiveDate, Utc};
use serde::Deserialize;
use serde_derive::Serialize;
use skippr_plugin_shared_api_source::{DateWindowPlanner, RetryConfig, RetryableHttpClient};
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
struct XAdsCheckpoint {
    last_completed_date: String,
}
#[derive(Debug, Clone, Deserialize)]
pub struct DataSourceXAdsPluginConfig {
    pub account_id: String,
    #[serde(default)]
    pub ad_account_id: Option<String>,
    #[serde(default)]
    pub bearer_token: Option<String>,
    #[serde(default)]
    pub access_token: Option<String>,
    #[serde(default)]
    pub oauth_consumer_key: Option<String>,
    #[serde(default)]
    pub oauth_consumer_secret: Option<String>,
    #[serde(default)]
    pub oauth_token: Option<String>,
    #[serde(default)]
    pub oauth_token_secret: Option<String>,
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
pub struct DataSourceXAdsPlugin {
    config: DataSourceXAdsPluginConfig,
    http: RetryableHttpClient,
    account_id: String,
    oauth: Option<XOAuth1Credentials>,
    bearer_token: Option<String>,
}
impl DataSourceXAdsPlugin {
    pub fn new(mut config: DataSourceXAdsPluginConfig) -> Result<Self, std::io::Error> {
        if config.account_id.trim().is_empty() {
            config.account_id = config.ad_account_id.clone().unwrap_or_default();
        }
        config.validate()?;
        let account_id = normalize_account_id(&config.account_id);
        let oauth = build_oauth(&config);
        let bearer_token = config
            .bearer_token
            .clone()
            .or(config.access_token.clone())
            .or_else(|| std::env::var("X_ADS_BEARER_TOKEN").ok());
        if oauth.is_none()
            && bearer_token
                .as_deref()
                .unwrap_or_default()
                .trim()
                .is_empty()
            && !std::env::var("SKIPPR_X_ADS_FIXTURE_DIR")
                .map(|d| !d.trim().is_empty())
                .unwrap_or(false)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "X Ads requires OAuth 1.0a credentials or bearer_token",
            ));
        }
        Ok(Self {
            config,
            http: RetryableHttpClient::new(RetryConfig::default()),
            account_id,
            oauth,
            bearer_token,
        })
    }
    fn selected_streams(&self) -> Vec<&'static XAdsStreamDef> {
        resolve_streams(self.config.stream_profile, self.config.streams.clone())
    }
    fn streams_for_run(&self, discover: bool) -> Vec<&'static XAdsStreamDef> {
        if discover {
            resolve_streams(StreamProfile::Minimal, None)
        } else {
            self.selected_streams()
        }
    }
    fn api_client(&self) -> XAdsApiClient {
        XAdsApiClient::new(
            self.http.clone(),
            self.account_id.clone(),
            self.oauth.clone(),
            self.bearer_token.clone(),
        )
    }
    fn namespace_contract(s: &XAdsStreamDef) -> SourceNamespaceContract {
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
            description: "X Ads API source contract".into(),
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
        format!("x_ads:{}:{}", self.account_id, ns)
    }
    fn load_last_completed(ctx: &dyn SourceSyncContext, key: &str) -> Option<NaiveDate> {
        load_checkpoint_payload::<XAdsCheckpoint>(ctx, key)
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
            &XAdsCheckpoint {
                last_completed_date: date.format("%Y-%m-%d").to_string(),
            },
        )
        .map_err(|e| std::io::Error::other(e.to_string()))?;
        ctx.store_checkpoint(key, &e).map_err(std::io::Error::other)
    }
    fn submit(
        &self,
        ctx: &dyn SourceSyncContext,
        s: &XAdsStreamDef,
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
                source_uri: format!("x-ads://{}", self.account_id),
                namespace: Some(s.namespace.to_string()),
                cdc_rows: None,
            }],
        )?;
        Ok(())
    }
}
fn env_field(config: &Option<String>, key: &str) -> Option<String> {
    config
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| {
            std::env::var(key)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
}

fn build_oauth(c: &DataSourceXAdsPluginConfig) -> Option<XOAuth1Credentials> {
    Some(XOAuth1Credentials {
        consumer_key: env_field(&c.oauth_consumer_key, "X_ADS_CONSUMER_KEY")?,
        consumer_secret: env_field(&c.oauth_consumer_secret, "X_ADS_CONSUMER_SECRET")?,
        token: env_field(&c.oauth_token, "X_ADS_OAUTH_TOKEN")?,
        token_secret: env_field(&c.oauth_token_secret, "X_ADS_OAUTH_TOKEN_SECRET")?,
    })
}
impl DataSourceXAdsPluginConfig {
    pub fn validate(&self) -> Result<(), std::io::Error> {
        if normalize_account_id(&self.account_id).is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "account_id is required",
            ));
        }
        DataSourceXAdsPlugin::parse_date(&self.start_date)?;
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
impl DataSource for DataSourceXAdsPlugin {
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
        let api = self.api_client();
        for stream in self.streams_for_run(discover) {
            match stream.kind {
                XAdsStreamKind::Analytics => {
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
                    let body = api.fetch(stream, None).await?;
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
    fn config_supports_oauth1() {
        let cfg = DataSourceXAdsPluginConfig {
            account_id: "abc".into(),
            ad_account_id: None,
            bearer_token: None,
            access_token: None,
            oauth_consumer_key: Some("ck".into()),
            oauth_consumer_secret: Some("cs".into()),
            oauth_token: Some("tk".into()),
            oauth_token_secret: Some("ts".into()),
            start_date: "2024-01-01".into(),
            end_date: None,
            lookback_days: 3,
            stream_profile: StreamProfile::Full,
            processing_lag_days: 1,
            streams: None,
        };
        let plugin = DataSourceXAdsPlugin::new(cfg).unwrap();
        assert!(plugin
            .source_namespace_contracts()
            .iter()
            .any(|c| c.namespace == "x_ads.promoted_posts"));
    }
}
