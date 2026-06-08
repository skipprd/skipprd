use chrono::NaiveDate;
use skippr_plugin_shared_api_source::{
    OAuth2RefreshTokenAuth, RetryableHttpClient, TokenPagination,
};
use tracing::warn;

use crate::streams::{render_gaql, GoogleAdsStreamDef, GoogleAdsStreamKind};

pub const GOOGLE_ADS_API_BASE: &str = "https://googleads.googleapis.com";
const DEFAULT_API_VERSION: &str = "v20";

pub fn normalize_customer_id(customer_id: &str) -> String {
    customer_id.trim().replace('-', "")
}

pub fn api_version_or_default(api_version: Option<&str>) -> String {
    api_version
        .filter(|v| !v.trim().is_empty())
        .map(|v| v.trim().trim_start_matches('/').to_string())
        .unwrap_or_else(|| DEFAULT_API_VERSION.to_string())
}

pub async fn bearer_token(
    access_token: Option<&str>,
    oauth: Option<&OAuth2RefreshTokenAuth>,
) -> Result<String, std::io::Error> {
    if let Some(token) = access_token.filter(|t| !t.trim().is_empty()) {
        return Ok(token.trim().to_string());
    }
    if let Ok(token) = std::env::var("GOOGLE_ADS_ACCESS_TOKEN") {
        if !token.trim().is_empty() {
            return Ok(token.trim().to_string());
        }
    }
    if std::env::var("SKIPPR_GOOGLE_ADS_FIXTURE_DIR")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
    {
        return Ok("fixture".into());
    }
    if let Some(oauth) = oauth {
        return oauth.refresh().await.map_err(std::io::Error::other);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "Google Ads requires access_token, GOOGLE_ADS_ACCESS_TOKEN, or OAuth refresh credentials",
    ))
}

#[derive(Clone)]
pub struct GoogleAdsApiClient {
    pub http: RetryableHttpClient,
    pub customer_id: String,
    pub login_customer_id: Option<String>,
    pub developer_token: String,
    pub api_version: String,
}

impl GoogleAdsApiClient {
    pub fn new(
        http: RetryableHttpClient,
        customer_id: String,
        login_customer_id: Option<String>,
        developer_token: String,
        api_version: String,
    ) -> Self {
        Self {
            http,
            customer_id: normalize_customer_id(&customer_id),
            login_customer_id: login_customer_id
                .map(|id| normalize_customer_id(&id))
                .filter(|id| !id.is_empty()),
            developer_token,
            api_version,
        }
    }

    pub fn search_url(&self) -> String {
        format!(
            "{GOOGLE_ADS_API_BASE}/{}/customers/{}/googleAds:searchStream",
            self.api_version, self.customer_id
        )
    }

    pub fn build_search_request(
        &self,
        stream: &GoogleAdsStreamDef,
        start_date: NaiveDate,
        end_date: NaiveDate,
        bearer: &str,
    ) -> reqwest::RequestBuilder {
        let gaql = render_gaql(
            stream.gaql,
            &start_date.format("%Y-%m-%d").to_string(),
            &end_date.format("%Y-%m-%d").to_string(),
        );
        let mut req = self
            .http
            .client
            .post(self.search_url())
            .bearer_auth(bearer)
            .header("developer-token", self.developer_token.as_str())
            .json(&serde_json::json!({ "query": gaql }));
        if let Some(login) = &self.login_customer_id {
            req = req.header("login-customer-id", login.as_str());
        }
        req
    }

    pub async fn search_stream(
        &self,
        stream: &GoogleAdsStreamDef,
        start_date: NaiveDate,
        end_date: NaiveDate,
        bearer: &str,
    ) -> Result<serde_json::Value, std::io::Error> {
        if let Ok(dir) = std::env::var("SKIPPR_GOOGLE_ADS_FIXTURE_DIR") {
            if let Some(body) = load_fixture(&dir, stream.namespace) {
                return Ok(body);
            }
        }
        let mut attempt = 0u32;
        loop {
            let response = self
                .build_search_request(stream, start_date, end_date, bearer)
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
                        .map_err(|e| std::io::Error::other(e.to_string()))
                }
                skippr_plugin_shared_api_source::RetryDecision::RetryAfter(delay) => {
                    attempt += 1;
                    if attempt >= self.http.config.max_attempts {
                        return Err(std::io::Error::other(format!(
                            "Google Ads API failed after {attempt}/{} attempts: HTTP {status}",
                            self.http.config.max_attempts
                        )));
                    }
                    warn!(attempt, %status, "Google Ads transient error; backing off");
                    self.http.backoff(attempt, delay).await;
                }
                skippr_plugin_shared_api_source::RetryDecision::GiveUp => {
                    let text = response.text().await.unwrap_or_default();
                    return Err(std::io::Error::other(format!(
                        "Google Ads API failed: HTTP {status} {text}"
                    )));
                }
            }
        }
    }
}

pub(crate) fn load_fixture(dir: &str, namespace: &str) -> Option<serde_json::Value> {
    let file = match namespace {
        "google_ads.account_daily" => "account_search_stream.json",
        "google_ads.campaign_daily" => "campaign_search_stream.json",
        "google_ads.ad_group_daily" => "ad_group_search_stream.json",
        "google_ads.keyword_daily" => "keyword_search_stream.json",
        "google_ads.search_term_daily" => "search_term_search_stream.json",
        "google_ads.landing_page_daily" => "landing_page_search_stream.json",
        _ => return None,
    };
    let path = format!("{}/{}", dir.trim_end_matches('/'), file);
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn rows_from_search_stream_body(body: &serde_json::Value) -> Vec<serde_json::Value> {
    if let Some(arr) = body.as_array() {
        return arr
            .iter()
            .flat_map(|chunk| {
                chunk
                    .get("results")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default()
            })
            .collect();
    }
    body.get("results")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

pub fn parse_google_ads_rows(
    body: &serde_json::Value,
    stream: &GoogleAdsStreamDef,
    customer_id: &str,
) -> Vec<serde_json::Value> {
    rows_from_search_stream_body(body)
        .into_iter()
        .map(|row| flatten_row(row, stream.kind, customer_id))
        .collect()
}

fn flatten_row(
    row: serde_json::Value,
    kind: GoogleAdsStreamKind,
    fallback_customer_id: &str,
) -> serde_json::Value {
    let mut record = serde_json::Map::new();
    let date = row
        .pointer("/segments/date")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if !date.is_empty() {
        record.insert("date".into(), serde_json::Value::String(date.to_string()));
    }
    let customer_id = row
        .pointer("/customer/id")
        .and_then(json_to_string)
        .unwrap_or_else(|| fallback_customer_id.to_string());
    record.insert("customer_id".into(), serde_json::Value::String(customer_id));
    copy_path(
        &row,
        &mut record,
        "/customer/descriptiveName",
        "customer_name",
    );
    copy_path(&row, &mut record, "/campaign/id", "campaign_id");
    copy_path(&row, &mut record, "/campaign/name", "campaign_name");
    copy_path(&row, &mut record, "/campaign/status", "campaign_status");
    copy_path(&row, &mut record, "/adGroup/id", "ad_group_id");
    copy_path(&row, &mut record, "/adGroup/name", "ad_group_name");
    copy_path(&row, &mut record, "/adGroup/status", "ad_group_status");
    copy_path(
        &row,
        &mut record,
        "/adGroupCriterion/criterionId",
        "criterion_id",
    );
    copy_path(
        &row,
        &mut record,
        "/adGroupCriterion/keyword/text",
        "keyword_text",
    );
    copy_path(
        &row,
        &mut record,
        "/adGroupCriterion/keyword/matchType",
        "keyword_match_type",
    );
    copy_path(
        &row,
        &mut record,
        "/searchTermView/searchTerm",
        "search_term",
    );
    copy_path(
        &row,
        &mut record,
        "/landingPageView/unexpandedFinalUrl",
        "landing_page_url",
    );
    for field in [
        "impressions",
        "clicks",
        "costMicros",
        "conversions",
        "ctr",
        "averageCpc",
    ] {
        if let Some(value) = row.pointer(&format!("/metrics/{field}")) {
            record.insert(to_snake(field), value.clone());
        }
    }
    if let Some(micros) = record.get("cost_micros") {
        if let Some(spend) = micros_to_spend(micros) {
            record.insert("spend".into(), serde_json::json!(spend));
        }
    }
    record.insert(
        "stream_kind".into(),
        serde_json::Value::String(format!("{:?}", kind)),
    );
    serde_json::Value::Object(record)
}

fn micros_to_spend(micros: &serde_json::Value) -> Option<f64> {
    let raw = micros
        .as_f64()
        .or_else(|| micros.as_i64().map(|n| n as f64))
        .or_else(|| micros.as_u64().map(|n| n as f64))
        .or_else(|| micros.as_str().and_then(|s| s.parse::<f64>().ok()))?;
    Some(raw / 1_000_000.0)
}

fn copy_path(
    row: &serde_json::Value,
    out: &mut serde_json::Map<String, serde_json::Value>,
    path: &str,
    key: &str,
) {
    if let Some(value) = row.pointer(path) {
        out.insert(key.into(), value.clone());
    }
}

fn json_to_string(value: &serde_json::Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_string)
        .or_else(|| value.as_i64().map(|n| n.to_string()))
        .or_else(|| value.as_u64().map(|n| n.to_string()))
}

fn to_snake(camel: &str) -> String {
    let mut out = String::new();
    for (i, ch) in camel.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

#[allow(dead_code)]
fn _pagination_marker(_: TokenPagination) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streams::CURATED_STREAMS;
    use skippr_plugin_shared_api_source::{RetryConfig, RetryableHttpClient};

    #[test]
    fn request_uses_google_ads_headers_and_login_customer() {
        let client = GoogleAdsApiClient::new(
            RetryableHttpClient::new(RetryConfig::default()),
            "123-456".into(),
            Some("999-888".into()),
            "dev".into(),
            "v17".into(),
        );
        let req = client
            .build_search_request(
                CURATED_STREAMS
                    .iter()
                    .find(|s| s.namespace == "google_ads.campaign_daily")
                    .unwrap(),
                NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(),
                NaiveDate::from_ymd_opt(2024, 1, 2).unwrap(),
                "token",
            )
            .build()
            .unwrap();
        assert_eq!(
            req.url().as_str(),
            "https://googleads.googleapis.com/v17/customers/123456/googleAds:searchStream"
        );
        assert_eq!(req.headers().get("developer-token").unwrap(), "dev");
        assert_eq!(req.headers().get("login-customer-id").unwrap(), "999888");
    }

    #[test]
    fn parser_flattens_gaql_result() {
        let body = serde_json::json!([{ "results": [{"segments":{"date":"2024-01-01"},"customer":{"id":"123"},"campaign":{"id":"55","name":"Brand"},"metrics":{"impressions":"10","clicks":"2","costMicros":"1000"}}]}]);
        let rows = parse_google_ads_rows(&body, &CURATED_STREAMS[1], "123");
        assert_eq!(rows[0]["campaign_id"], "55");
        assert_eq!(rows[0]["cost_micros"], "1000");
        assert_eq!(rows[0]["spend"], 0.001);
    }

    #[test]
    fn fixture_campaign_search_stream_parses() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        std::env::set_var("SKIPPR_GOOGLE_ADS_FIXTURE_DIR", dir);
        let stream = CURATED_STREAMS
            .iter()
            .find(|s| s.namespace == "google_ads.campaign_daily")
            .unwrap();
        let _client = GoogleAdsApiClient::new(
            RetryableHttpClient::new(RetryConfig::default()),
            "1234567890".into(),
            None,
            "dev".into(),
            "v17".into(),
        );
        let body = load_fixture(dir, stream.namespace).expect("fixture");
        let rows = parse_google_ads_rows(&body, stream, "1234567890");
        assert!(!rows.is_empty());
        assert_eq!(rows[0]["campaign_name"], "Brand Search");
        assert!(rows[0].get("spend").is_some());
        std::env::remove_var("SKIPPR_GOOGLE_ADS_FIXTURE_DIR");
    }
}
