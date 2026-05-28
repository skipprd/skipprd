use chrono::NaiveDate;
use skippr_plugin_shared_api_source::{RetryableHttpClient, TokenPagination};
use tracing::warn;

use crate::streams::{insights_level_param, MetaStreamDef};

pub const META_GRAPH_API_BASE: &str = "https://graph.facebook.com";

const DEFAULT_API_VERSION: &str = "v21.0";

const BASE_INSIGHT_FIELDS: &str = "impressions,clicks,spend,reach,frequency,cpm,cpc,ctr,\
account_id,account_name,campaign_id,campaign_name,adset_id,adset_name,ad_id,ad_name,actions";

const PLACEMENT_EXTRA_FIELDS: &str = "publisher_platform,platform_position";

pub fn normalize_ad_account_id(ad_account_id: &str) -> String {
    ad_account_id
        .trim()
        .trim_start_matches("act_")
        .to_string()
}

pub fn api_version_or_default(api_version: Option<&str>) -> String {
    api_version
        .filter(|v| !v.trim().is_empty())
        .map(|v| v.trim().trim_start_matches('/').to_string())
        .unwrap_or_else(|| DEFAULT_API_VERSION.to_string())
}

pub fn insights_fields(stream: &MetaStreamDef) -> String {
    if stream.placement_breakdown {
        format!("{BASE_INSIGHT_FIELDS},{PLACEMENT_EXTRA_FIELDS}")
    } else {
        BASE_INSIGHT_FIELDS.to_string()
    }
}

pub fn instagram_filter_json() -> &'static str {
    r#"[{"field":"publisher_platform","operator":"IN","value":["instagram"]}]"#
}

#[derive(Clone)]
pub struct MetaInsightsApiClient {
    pub http: RetryableHttpClient,
    pub ad_account_id: String,
    pub api_version: String,
    pub instagram_filter: bool,
}

impl MetaInsightsApiClient {
    pub fn new(
        http: RetryableHttpClient,
        ad_account_id: String,
        api_version: String,
        instagram_filter: bool,
    ) -> Self {
        Self {
            http,
            ad_account_id: normalize_ad_account_id(&ad_account_id),
            api_version,
            instagram_filter,
        }
    }

    fn insights_base_url(&self) -> String {
        format!(
            "{META_GRAPH_API_BASE}/{}/act_{}/insights",
            self.api_version, self.ad_account_id
        )
    }

    fn build_insights_request(
        &self,
        stream: &MetaStreamDef,
        date: NaiveDate,
    ) -> reqwest::RequestBuilder {
        let day = date.format("%Y-%m-%d").to_string();
        let time_range = serde_json::json!({"since": day, "until": day}).to_string();
        let mut req = self.http.client.get(self.insights_base_url()).query(&[
            ("time_range", time_range.as_str()),
            ("time_increment", "1"),
            ("level", insights_level_param(stream.level)),
            ("fields", insights_fields(stream).as_str()),
        ]);
        if self.instagram_filter {
            req = req.query(&[("filtering", instagram_filter_json())]);
        }
        if stream.placement_breakdown {
            req = req.query(&[("breakdowns", "platform_position")]);
        }
        req
    }

    pub async fn fetch_insights_all_pages(
        &self,
        stream: &MetaStreamDef,
        date: NaiveDate,
        auth_header: &str,
    ) -> Result<serde_json::Value, std::io::Error> {
        if let Ok(dir) = std::env::var("SKIPPR_META_INSTAGRAM_ADS_FIXTURE_DIR") {
            if let Some(body) = load_fixture_insights(&dir, stream.namespace) {
                return Ok(body);
            }
        }

        let mut merged: Vec<serde_json::Value> = Vec::new();
        let mut pagination = TokenPagination::default();

        let url = self
            .resolve_insights_url(stream, date, pagination.next_token.as_deref())
            .await?;
        let mut last_body = self.get_url_with_retry(auth_header, &url).await?;
        if let Some(page) = last_body.get("data").and_then(|v| v.as_array()) {
            merged.extend(page.clone());
        }
        pagination.next_token = last_body
            .pointer("/paging/next")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        while pagination.should_continue() {
            let url = self
                .resolve_insights_url(stream, date, pagination.next_token.as_deref())
                .await?;
            last_body = self.get_url_with_retry(auth_header, &url).await?;
            if let Some(page) = last_body.get("data").and_then(|v| v.as_array()) {
                merged.extend(page.clone());
            }
            pagination.next_token = last_body
                .pointer("/paging/next")
                .and_then(|v| v.as_str())
                .map(str::to_string);
        }

        if let Some(data) = last_body.get_mut("data") {
            *data = serde_json::Value::Array(merged);
            Ok(last_body)
        } else {
            Ok(serde_json::json!({ "data": merged }))
        }
    }

    async fn resolve_insights_url(
        &self,
        stream: &MetaStreamDef,
        date: NaiveDate,
        after_url: Option<&str>,
    ) -> Result<String, std::io::Error> {
        if let Some(url) = after_url.filter(|u| !u.trim().is_empty()) {
            return Ok(url.to_string());
        }
        let request = self
            .build_insights_request(stream, date)
            .build()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(request.url().to_string())
    }

    async fn get_url_with_retry(
        &self,
        auth_header: &str,
        url: &str,
    ) -> Result<serde_json::Value, std::io::Error> {
        let mut attempt = 0u32;
        loop {
            let response = self
                .http
                .client
                .get(url)
                .header("Authorization", auth_header)
                .send()
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            let status = response.status();
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            match RetryableHttpClient::classify_status(status, retry_after) {
                skippr_plugin_shared_api_source::RetryDecision::Success => {
                    return response
                        .json()
                        .await
                        .map_err(|e| std::io::Error::other(e.to_string()));
                }
                skippr_plugin_shared_api_source::RetryDecision::RetryAfter(delay) => {
                    attempt += 1;
                    if attempt >= self.http.config.max_attempts {
                        return Err(std::io::Error::other(format!(
                            "Meta Marketing API failed after {attempt}/{} attempts: HTTP {status}",
                            self.http.config.max_attempts
                        )));
                    }
                    warn!(
                        attempt,
                        %status,
                        %url,
                        "Meta Marketing API transient error; backing off"
                    );
                    self.http.backoff(attempt, delay).await;
                }
                skippr_plugin_shared_api_source::RetryDecision::GiveUp => {
                    let text = response.text().await.unwrap_or_default();
                    return Err(std::io::Error::other(format!(
                        "Meta Marketing API failed: HTTP {status} {text}"
                    )));
                }
            }
        }
    }
}

fn load_fixture_insights(dir: &str, namespace: &str) -> Option<serde_json::Value> {
    let file = match namespace {
        "meta_instagram_ads.account_daily" => "account_insights.json",
        "meta_instagram_ads.campaign_daily" => "campaign_insights.json",
        "meta_instagram_ads.adset_daily" => "adset_insights.json",
        "meta_instagram_ads.ad_daily" => "ad_insights.json",
        "meta_instagram_ads.campaign_placement_daily" => "campaign_placement_insights.json",
        _ => return None,
    };
    let path = format!("{}/{}", dir.trim_end_matches('/'), file);
    let bytes = std::fs::read(&path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn rows_from_insights_body(body: &serde_json::Value) -> Vec<serde_json::Value> {
    body.get("data")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streams::{streams_for_profile, StreamProfile, CURATED_STREAMS};
    use chrono::NaiveDate;
    use skippr_plugin_shared_api_source::{RetryConfig, RetryableHttpClient};

    #[test]
    fn insights_base_url_normalizes_act_prefix() {
        let client = MetaInsightsApiClient::new(
            RetryableHttpClient::new(RetryConfig::default()),
            "123".into(),
            "v21.0".into(),
            true,
        );
        assert!(client.insights_base_url().contains("/act_123/insights"));
        let prefixed = MetaInsightsApiClient::new(
            RetryableHttpClient::new(RetryConfig::default()),
            "act_456".into(),
            "v21.0".into(),
            true,
        );
        assert!(prefixed.insights_base_url().contains("/act_456/insights"));
    }

    #[test]
    fn instagram_filter_json_targets_instagram_only() {
        let filter: serde_json::Value = serde_json::from_str(instagram_filter_json()).unwrap();
        assert_eq!(filter[0]["field"], "publisher_platform");
        assert_eq!(filter[0]["value"], serde_json::json!(["instagram"]));
    }

    #[test]
    fn insights_request_includes_instagram_filter_when_enabled() {
        let client = MetaInsightsApiClient::new(
            RetryableHttpClient::new(RetryConfig::default()),
            "123".into(),
            "v21.0".into(),
            true,
        );
        let stream = streams_for_profile(StreamProfile::Minimal)[0];
        let date = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
        let req = client
            .build_insights_request(stream, date)
            .build()
            .expect("request");
        let query: Vec<(String, String)> = req
            .url()
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        let filtering = query
            .iter()
            .find(|(k, _)| k == "filtering")
            .map(|(_, v)| v.as_str())
            .expect("filtering query param");
        assert_eq!(filtering, instagram_filter_json());
    }

    #[test]
    fn insights_request_omits_filter_when_disabled() {
        let client = MetaInsightsApiClient::new(
            RetryableHttpClient::new(RetryConfig::default()),
            "123".into(),
            "v21.0".into(),
            false,
        );
        let stream = streams_for_profile(StreamProfile::Minimal)[0];
        let date = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
        let req = client
            .build_insights_request(stream, date)
            .build()
            .expect("request");
        assert!(!req.url().query().unwrap_or("").contains("filtering"));
    }

    #[test]
    fn rows_from_insights_body_handles_empty_and_malformed() {
        assert!(rows_from_insights_body(&serde_json::json!({})).is_empty());
        assert!(rows_from_insights_body(&serde_json::json!({"data": "bad"})).is_empty());
        assert!(rows_from_insights_body(&serde_json::json!({"data": []})).is_empty());
    }

    #[test]
    fn meta_api_classifies_rate_limit_for_retry() {
        use skippr_plugin_shared_api_source::RetryDecision;
        assert_eq!(
            RetryableHttpClient::classify_status(reqwest::StatusCode::TOO_MANY_REQUESTS, None),
            RetryDecision::RetryAfter(std::time::Duration::from_secs(30))
        );
        assert_eq!(
            RetryableHttpClient::classify_status(reqwest::StatusCode::BAD_REQUEST, None),
            RetryDecision::GiveUp
        );
    }

    #[test]
    fn load_fixture_insights_reads_account_file() {
        static FIXTURE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _lock = FIXTURE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let fixture_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        std::env::set_var("SKIPPR_META_INSTAGRAM_ADS_FIXTURE_DIR", fixture_dir);
        let client = MetaInsightsApiClient::new(
            RetryableHttpClient::new(RetryConfig::default()),
            "123".into(),
            "v21.0".into(),
            true,
        );
        let stream = CURATED_STREAMS[0];
        let rt = tokio::runtime::Runtime::new().unwrap();
        let body = rt
            .block_on(client.fetch_insights_all_pages(
                &stream,
                NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(),
                "Bearer fixture",
            ))
            .expect("fixture insights");
        assert!(!rows_from_insights_body(&body).is_empty());
        std::env::remove_var("SKIPPR_META_INSTAGRAM_ADS_FIXTURE_DIR");
    }
}
