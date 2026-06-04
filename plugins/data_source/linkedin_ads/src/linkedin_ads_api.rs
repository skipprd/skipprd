use chrono::{Datelike, NaiveDate};
use skippr_plugin_shared_api_source::{RetryableHttpClient, TokenPagination};
use tracing::warn;

use crate::streams::{LinkedInStreamDef, LinkedInStreamKind};

pub const LINKEDIN_API_BASE: &str = "https://api.linkedin.com/rest";
const DEFAULT_REST_VERSION: &str = "202411";

pub fn normalize_account_id(account_id: &str) -> String {
    account_id
        .trim()
        .trim_start_matches("urn:li:sponsoredAccount:")
        .to_string()
}
pub fn account_urn(account_id: &str) -> String {
    format!(
        "urn:li:sponsoredAccount:{}",
        normalize_account_id(account_id)
    )
}
pub fn rest_version_or_default(version: Option<&str>) -> String {
    version
        .filter(|v| !v.trim().is_empty())
        .unwrap_or(DEFAULT_REST_VERSION)
        .trim()
        .to_string()
}

#[derive(Clone)]
pub struct LinkedInAdsApiClient {
    pub http: RetryableHttpClient,
    pub account_id: String,
    pub rest_version: String,
}

impl LinkedInAdsApiClient {
    pub fn new(http: RetryableHttpClient, account_id: String, rest_version: String) -> Self {
        Self {
            http,
            account_id: normalize_account_id(&account_id),
            rest_version,
        }
    }
    pub fn build_request(
        &self,
        stream: &LinkedInStreamDef,
        date: Option<NaiveDate>,
        token: &str,
    ) -> reqwest::RequestBuilder {
        let mut req = self
            .http
            .client
            .get(self.url_for(stream, date))
            .bearer_auth(token)
            .header("LinkedIn-Version", self.rest_version.as_str())
            .header("X-Restli-Protocol-Version", "2.0.0");
        if matches!(stream.kind, LinkedInStreamKind::AdAnalytics) {
            req = req.header("Accept", "application/json");
        }
        req
    }
    pub fn url_for(&self, stream: &LinkedInStreamDef, date: Option<NaiveDate>) -> String {
        let acct = account_urn(&self.account_id);
        match stream.kind {
            LinkedInStreamKind::AdAccounts => format!("{LINKEDIN_API_BASE}/adAccounts?q=search&search=(status:(values:List(ACTIVE,DRAFT,PAUSED)))"),
            LinkedInStreamKind::CampaignGroups => format!("{LINKEDIN_API_BASE}/adCampaignGroups?q=search&search=(account:(values:List({acct})))"),
            LinkedInStreamKind::Campaigns => format!("{LINKEDIN_API_BASE}/adCampaigns?q=search&search=(account:(values:List({acct})))"),
            LinkedInStreamKind::Creatives => format!("{LINKEDIN_API_BASE}/adCreatives?q=search&search=(account:(values:List({acct})))"),
            LinkedInStreamKind::AdAnalytics => {
                let d = date.expect("analytics date required");
                format!("{LINKEDIN_API_BASE}/adAnalytics?q=analytics&pivot=CAMPAIGN&timeGranularity=DAILY&accounts=List({acct})&dateRange=(start:(year:{},month:{},day:{}),end:(year:{},month:{},day:{}))", d.year(), d.month(), d.day(), d.year(), d.month(), d.day())
            }
        }
    }
    pub async fn fetch(
        &self,
        stream: &LinkedInStreamDef,
        date: Option<NaiveDate>,
        token: &str,
    ) -> Result<serde_json::Value, std::io::Error> {
        if let Ok(dir) = std::env::var("SKIPPR_LINKEDIN_ADS_FIXTURE_DIR") {
            if !dir.trim().is_empty() {
                return load_fixture(&dir, stream.namespace).ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!(
                            "fixture missing for {} in {}",
                            stream.namespace, dir
                        ),
                    )
                });
            }
        }
        let mut merged = Vec::new();
        let mut pagination = TokenPagination::default();
        let mut next = Some(self.url_for(stream, date));
        let mut last = serde_json::json!({});
        while let Some(url) = next {
            last = self.get_url(&url, token).await?;
            if let Some(rows) = rows_from_body(&last).as_array() {
                merged.extend(rows.clone());
            }
            pagination.next_token = last
                .pointer("/paging/links")
                .and_then(|v| v.as_array())
                .and_then(|links| {
                    links
                        .iter()
                        .find(|l| l.get("rel").and_then(|r| r.as_str()) == Some("next"))
                })
                .and_then(|l| l.get("href"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            next = pagination.next_token.clone();
            if !pagination.should_continue() {
                break;
            }
        }
        if merged.is_empty() {
            Ok(last)
        } else {
            Ok(serde_json::json!({"elements": merged}))
        }
    }
    async fn get_url(&self, url: &str, token: &str) -> Result<serde_json::Value, std::io::Error> {
        let mut attempt = 0u32;
        loop {
            let response = self
                .http
                .client
                .get(url)
                .bearer_auth(token)
                .header("LinkedIn-Version", self.rest_version.as_str())
                .header("X-Restli-Protocol-Version", "2.0.0")
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
                        return Err(std::io::Error::other(format!("LinkedIn Marketing API failed after {attempt}/{} attempts: HTTP {status}", self.http.config.max_attempts)));
                    }
                    warn!(attempt, %status, "LinkedIn transient error; backing off");
                    self.http.backoff(attempt, delay).await;
                }
                skippr_plugin_shared_api_source::RetryDecision::GiveUp => {
                    let text = response.text().await.unwrap_or_default();
                    return Err(std::io::Error::other(format!(
                        "LinkedIn Marketing API failed: HTTP {status} {text}"
                    )));
                }
            }
        }
    }
}

fn load_fixture(dir: &str, namespace: &str) -> Option<serde_json::Value> {
    let file = match namespace {
        "linkedin_ads.ad_accounts" => "ad_accounts.json",
        "linkedin_ads.campaign_groups" => "campaign_groups.json",
        "linkedin_ads.campaigns" => "campaigns.json",
        "linkedin_ads.creatives" => "creatives.json",
        "linkedin_ads.ad_analytics_daily" => "ad_analytics.json",
        _ => return None,
    };
    let bytes = std::fs::read(format!("{}/{}", dir.trim_end_matches('/'), file)).ok()?;
    serde_json::from_slice(&bytes).ok()
}
pub fn rows_from_body(body: &serde_json::Value) -> serde_json::Value {
    body.get("elements")
        .cloned()
        .or_else(|| body.get("data").cloned())
        .unwrap_or_else(|| serde_json::json!([]))
}
pub fn parse_rows(
    body: &serde_json::Value,
    stream: &LinkedInStreamDef,
    account_id: &str,
    date: Option<NaiveDate>,
) -> Vec<serde_json::Value> {
    rows_from_body(body)
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|row| normalize_row(row, stream.kind, account_id, date))
        .collect()
}
fn normalize_row(
    row: serde_json::Value,
    kind: LinkedInStreamKind,
    account_id: &str,
    date: Option<NaiveDate>,
) -> serde_json::Value {
    let mut obj = row.as_object().cloned().unwrap_or_default();
    obj.insert(
        "account_urn".into(),
        serde_json::Value::String(account_urn(account_id)),
    );
    if let Some(d) = date {
        obj.insert(
            "date".into(),
            serde_json::Value::String(d.format("%Y-%m-%d").to_string()),
        );
    }
    let urn_key = match kind {
        LinkedInStreamKind::AdAccounts => "account_urn",
        LinkedInStreamKind::CampaignGroups => "campaign_group_urn",
        LinkedInStreamKind::Campaigns => "campaign_urn",
        LinkedInStreamKind::Creatives => "creative_urn",
        LinkedInStreamKind::AdAnalytics => "pivot_value",
    };
    if !obj.contains_key(urn_key) {
        if let Some(id) = obj
            .get("id")
            .cloned()
            .or_else(|| obj.get("pivotValue").cloned())
        {
            obj.insert(urn_key.into(), id);
        }
    }
    if matches!(kind, LinkedInStreamKind::AdAnalytics) {
        if let Some(cost) = obj.get("costInUsd").and_then(|v| v.as_str()) {
            if let Ok(spend) = cost.parse::<f64>() {
                obj.insert("spend".into(), serde_json::json!(spend));
            }
        } else if let Some(cost) = obj.get("costInUsd").and_then(|v| v.as_f64()) {
            obj.insert("spend".into(), serde_json::json!(cost));
        }
        let campaign_urn = obj
            .get("pivot_value")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        obj.entry("campaign_urn").or_insert(campaign_urn);
    }
    serde_json::Value::Object(obj)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streams::CURATED_STREAMS;
    use skippr_plugin_shared_api_source::{RetryConfig, RetryableHttpClient};
    #[test]
    fn fixture_sync_produces_rows() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        std::env::set_var("SKIPPR_LINKEDIN_ADS_FIXTURE_DIR", dir);
        let client = LinkedInAdsApiClient::new(
            RetryableHttpClient::new(RetryConfig::default()),
            "123".into(),
            "202411".into(),
        );
        let campaign_stream = &CURATED_STREAMS[2];
        let rt = tokio::runtime::Runtime::new().unwrap();
        let campaigns = rt
            .block_on(client.fetch(campaign_stream, None, "fixture"))
            .unwrap();
        let rows = parse_rows(&campaigns, campaign_stream, "123", None);
        assert_eq!(rows.len(), 2);
        let analytics_stream = &CURATED_STREAMS[4];
        let sample_date = NaiveDate::from_ymd_opt(2024, 1, 15).unwrap();
        let analytics = rt
            .block_on(client.fetch(analytics_stream, Some(sample_date), "fixture"))
            .unwrap();
        let arows = parse_rows(&analytics, analytics_stream, "123", Some(sample_date));
        assert_eq!(arows.len(), 1);
        assert!(arows[0].get("spend").and_then(|v| v.as_f64()).unwrap() > 0.0);
        std::env::remove_var("SKIPPR_LINKEDIN_ADS_FIXTURE_DIR");
    }

    #[test]
    fn request_has_linkedin_version_headers() {
        let client = LinkedInAdsApiClient::new(
            RetryableHttpClient::new(RetryConfig::default()),
            "123".into(),
            "202411".into(),
        );
        let req = client
            .build_request(
                &CURATED_STREAMS[4],
                Some(NaiveDate::from_ymd_opt(2024, 1, 2).unwrap()),
                "token",
            )
            .build()
            .unwrap();
        assert_eq!(req.headers().get("LinkedIn-Version").unwrap(), "202411");
        assert!(req.url().as_str().contains("/adAnalytics"));
        assert!(req.url().as_str().contains("urn:li:sponsoredAccount:123"));
    }
}
