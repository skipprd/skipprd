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

use crate::google_ads_api::{
    api_version_or_default, bearer_token, normalize_customer_id, parse_google_ads_rows,
    GoogleAdsApiClient,
};
use crate::streams::{resolve_streams, GoogleAdsStreamDef, StreamProfile};

const CHECKPOINT_PAYLOAD_VERSION: u32 = 1;
const DISCOVER_SAMPLE_DAYS: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct GoogleAdsNamespaceCheckpoint {
    last_completed_date: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DataSourceGoogleAdsPluginConfig {
    pub customer_id: String,
    pub developer_token: String,
    #[serde(default)]
    pub login_customer_id: Option<String>,
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

pub struct DataSourceGoogleAdsPlugin {
    config: DataSourceGoogleAdsPluginConfig,
    http: RetryableHttpClient,
    oauth: Option<OAuth2RefreshTokenAuth>,
    customer_id: String,
}

impl DataSourceGoogleAdsPlugin {
    pub fn new(config: DataSourceGoogleAdsPluginConfig) -> Result<Self, std::io::Error> {
        config.validate()?;
        let oauth = build_oauth(&config);
        let customer_id = normalize_customer_id(&config.customer_id);
        Ok(Self {
            config,
            http: RetryableHttpClient::new(RetryConfig::default()),
            oauth,
            customer_id,
        })
    }

    fn selected_streams(&self) -> Vec<&'static GoogleAdsStreamDef> {
        resolve_streams(self.config.stream_profile, self.config.streams.clone())
    }

    fn streams_for_run(&self, discover: bool) -> Vec<&'static GoogleAdsStreamDef> {
        if discover {
            resolve_streams(StreamProfile::Minimal, None)
        } else {
            self.selected_streams()
        }
    }

    fn api_client(&self) -> GoogleAdsApiClient {
        GoogleAdsApiClient::new(
            self.http.clone(),
            self.customer_id.clone(),
            self.config.login_customer_id.clone(),
            self.config.developer_token.trim().to_string(),
            api_version_or_default(self.config.api_version.as_deref()),
        )
    }

    fn namespace_contract(stream: &GoogleAdsStreamDef) -> SourceNamespaceContract {
        SourceNamespaceContract {
            namespace: stream.namespace.to_string(),
            primary_key: stream
                .primary_key
                .iter()
                .map(|k| FieldPath::single(*k))
                .collect(),
            cursor: Some(FieldPath::single("date")),
            partition_key: vec![FieldPath::single("date")],
            write_policy: WritePolicy::ReplacePartition,
            refresh_window: None,
            description: "Google Ads mutable daily GAQL report".into(),
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
        Ok(configured
            .min(Utc::now().date_naive() - Duration::days(self.config.processing_lag_days as i64)))
    }

    fn checkpoint_key(&self, namespace: &str) -> String {
        format!("google_ads:{}:{}", self.customer_id, namespace)
    }

    fn load_last_completed(ctx: &dyn SourceSyncContext, key: &str) -> Option<NaiveDate> {
        load_checkpoint_payload::<GoogleAdsNamespaceCheckpoint>(ctx, key)
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
            &GoogleAdsNamespaceCheckpoint {
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
        stream: &GoogleAdsStreamDef,
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
        submit_payload_batches(
            ctx,
            vec![IngestBatch {
                offset_key: OffsetKey::new(stream.namespace, date.format("%Y-%m-%d").to_string()),
                data: payload,
                bytes,
                offset_pos: None,
                source_uri: format!(
                    "google-ads://customers/{}/{}",
                    self.customer_id, stream.namespace
                ),
                namespace: Some(stream.namespace.to_string()),
                cdc_rows: None,
            }],
        )?;
        Ok(())
    }

    async fn sync_stream_date(
        &self,
        api: &GoogleAdsApiClient,
        stream: &GoogleAdsStreamDef,
        date: NaiveDate,
        token: &str,
    ) -> Result<Vec<serde_json::Value>, std::io::Error> {
        let body = api.search_stream(stream, date, date, token).await?;
        Ok(parse_google_ads_rows(&body, stream, &self.customer_id))
    }
}

fn build_oauth(config: &DataSourceGoogleAdsPluginConfig) -> Option<OAuth2RefreshTokenAuth> {
    Some(OAuth2RefreshTokenAuth::new(
        config.oauth_token_url.as_deref()?,
        config.oauth_client_id.as_deref()?,
        config.oauth_client_secret.as_deref()?,
        config.oauth_refresh_token.as_deref()?,
    ))
}

impl DataSourceGoogleAdsPluginConfig {
    pub fn validate(&self) -> Result<(), std::io::Error> {
        if normalize_customer_id(&self.customer_id).is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "customer_id is required",
            ));
        }
        if self.developer_token.trim().is_empty()
            && !std::env::var("SKIPPR_GOOGLE_ADS_FIXTURE_DIR")
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "developer_token is required",
            ));
        }
        Self::parse_start_date(&self.start_date)?;
        Ok(())
    }
    fn parse_start_date(value: &str) -> Result<NaiveDate, std::io::Error> {
        DataSourceGoogleAdsPlugin::parse_date(value)
    }
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

fn group_rows_by_date(rows: Vec<serde_json::Value>) -> HashMap<String, Vec<serde_json::Value>> {
    let mut grouped = HashMap::new();
    for row in rows {
        let date = row
            .get("date")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        grouped.entry(date).or_insert_with(Vec::new).push(row);
    }
    grouped
}

#[async_trait]
impl DataSource for DataSourceGoogleAdsPlugin {
    fn execution_contract(&self) -> SourceExecutionContract {
        SourceExecutionContract::stream(SourceOnceContract::Finite)
    }

    fn source_namespace_contracts(&self) -> Vec<SourceNamespaceContract> {
        self.selected_streams()
            .iter()
            .map(|stream| {
                let mut contract = Self::namespace_contract(stream);
                contract.refresh_window = Some(self.config.lookback_days);
                contract
            })
            .collect()
    }

    async fn sync(&mut self, ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error> {
        let discover = runtime_is_discover_mode();
        let start_date = Self::parse_date(&self.config.start_date)?;
        let mut end_date = self.effective_end_date()?;
        if discover {
            let (s, e) = discover_sample_date_window(end_date);
            end_date = e.max(s);
        }
        let token = bearer_token(self.config.access_token.as_deref(), self.oauth.as_ref()).await?;
        let api = self.api_client();
        for stream in self.streams_for_run(discover) {
            let key = self.checkpoint_key(stream.namespace);
            let last = if discover {
                None
            } else {
                Self::load_last_completed(ctx.as_ref(), &key)
            };
            let window = DateWindowPlanner {
                lookback_days: if discover {
                    0
                } else {
                    self.config.lookback_days
                },
            }
            .plan(start_date, last, end_date);
            for date in DateWindowPlanner::dates_inclusive(&window) {
                let rows = self.sync_stream_date(&api, stream, date, &token).await?;
                let grouped = group_rows_by_date(rows);
                self.submit_rows_for_date(
                    ctx.as_ref(),
                    stream,
                    date,
                    grouped
                        .get(&date.format("%Y-%m-%d").to_string())
                        .cloned()
                        .unwrap_or_default(),
                )?;
                if !discover {
                    Self::store_last_completed(ctx.as_ref(), &key, date)?;
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
    fn config_requires_google_ads_account_fields() {
        let cfg = DataSourceGoogleAdsPluginConfig {
            customer_id: "123-456".into(),
            developer_token: "dev".into(),
            login_customer_id: Some("999-888".into()),
            access_token: Some("token".into()),
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
            streams: None,
        };
        let plugin = DataSourceGoogleAdsPlugin::new(cfg).unwrap();
        assert_eq!(plugin.customer_id, "123456");
        assert_eq!(
            plugin.source_namespace_contracts()[0].namespace,
            "google_ads.account_daily"
        );
    }
}
