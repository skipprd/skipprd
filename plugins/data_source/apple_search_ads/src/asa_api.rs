use chrono::NaiveDate;
use skippr_plugin_shared_api_source::{
    AppleAdsClientCredentialsAuth, OffsetPagination, RetryableHttpClient,
};
use tracing::warn;

use crate::streams::{AsaStreamDef, ReportGrain};

pub const ASA_API_BASE: &str = "https://api.searchads.apple.com/api/v5";
const DEFAULT_PAGE_LIMIT: u64 = 1000;

#[derive(Clone, Debug)]
pub struct CampaignRef {
    pub id: i64,
    #[allow(dead_code)]
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct AdGroupRef {
    #[allow(dead_code)]
    pub campaign_id: i64,
    pub id: i64,
    #[allow(dead_code)]
    pub name: String,
}

#[derive(Clone)]
pub struct AsaApiClient {
    pub http: RetryableHttpClient,
    pub org_id: String,
    pub time_zone: String,
    pub return_records_with_no_metrics: bool,
    auth: Option<AppleAdsClientCredentialsAuth>,
    access_token_override: Option<String>,
}

impl AsaApiClient {
    pub fn new(
        http: RetryableHttpClient,
        org_id: String,
        time_zone: String,
        return_records_with_no_metrics: bool,
        auth: Option<AppleAdsClientCredentialsAuth>,
        access_token_override: Option<String>,
    ) -> Self {
        Self {
            http,
            org_id,
            time_zone,
            return_records_with_no_metrics,
            auth,
            access_token_override,
        }
    }

    pub async fn authorization_header(&self) -> Result<String, std::io::Error> {
        if let Some(token) = self
            .access_token_override
            .as_deref()
            .filter(|t| !t.trim().is_empty())
        {
            return Ok(format!("Bearer {}", token.trim()));
        }
        if let Ok(dir) = std::env::var("SKIPPR_APPLE_SEARCH_ADS_FIXTURE_DIR") {
            if !dir.trim().is_empty() {
                return Ok("Bearer fixture".into());
            }
        }
        let auth = self.auth.as_ref().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Apple Search Ads requires client credentials (client_id, team_id, key_id, \
                 private key) or access_token / APPLE_SEARCH_ADS_ACCESS_TOKEN",
            )
        })?;
        let token = auth.access_token().await.map_err(std::io::Error::other)?;
        Ok(format!("Bearer {token}"))
    }

    fn org_context_header(&self) -> String {
        format!("orgId={}", self.org_id.trim())
    }

    pub async fn list_campaigns(&self) -> Result<Vec<CampaignRef>, std::io::Error> {
        if let Ok(dir) = std::env::var("SKIPPR_APPLE_SEARCH_ADS_FIXTURE_DIR") {
            let path = format!("{}/campaigns_list.json", dir.trim_end_matches('/'));
            if let Ok(bytes) = std::fs::read(&path) {
                let body: serde_json::Value =
                    serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
                return Ok(parse_campaign_list(&body));
            }
        }

        let auth = self.authorization_header().await?;
        let mut campaigns = Vec::new();
        let mut pagination = OffsetPagination {
            offset: 0,
            limit: DEFAULT_PAGE_LIMIT,
        };

        loop {
            let url = format!(
                "{ASA_API_BASE}/campaigns?limit={}&offset={}",
                pagination.limit, pagination.offset
            );
            let response = self
                .post_or_get_with_retry(&auth, reqwest::Method::GET, &url, None)
                .await?;
            let page = parse_campaign_list(&response);
            let count = page.len();
            campaigns.extend(page);
            if pagination.should_stop(count) {
                break;
            }
            pagination.advance(count);
        }
        Ok(campaigns)
    }

    pub async fn list_ad_groups(
        &self,
        campaign_id: i64,
    ) -> Result<Vec<AdGroupRef>, std::io::Error> {
        if let Ok(dir) = std::env::var("SKIPPR_APPLE_SEARCH_ADS_FIXTURE_DIR") {
            let path = format!("{}/ad_groups_list.json", dir.trim_end_matches('/'));
            if let Ok(bytes) = std::fs::read(&path) {
                let body: serde_json::Value =
                    serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
                return Ok(parse_ad_group_list(&body, campaign_id));
            }
        }

        let auth = self.authorization_header().await?;
        let mut ad_groups = Vec::new();
        let mut pagination = OffsetPagination {
            offset: 0,
            limit: DEFAULT_PAGE_LIMIT,
        };
        let url = format!("{ASA_API_BASE}/campaigns/{campaign_id}/adgroups");

        loop {
            let url = format!(
                "{url}?limit={}&offset={}",
                pagination.limit, pagination.offset
            );
            let response = self
                .post_or_get_with_retry(&auth, reqwest::Method::GET, &url, None)
                .await?;
            let page = parse_ad_group_list(&response, campaign_id);
            let count = page.len();
            ad_groups.extend(page);
            if pagination.should_stop(count) {
                break;
            }
            pagination.advance(count);
        }
        Ok(ad_groups)
    }

    pub async fn fetch_report_page(
        &self,
        stream: &AsaStreamDef,
        date: NaiveDate,
        campaign_id: Option<i64>,
        ad_group_id: Option<i64>,
        offset: u64,
        limit: u64,
    ) -> Result<serde_json::Value, std::io::Error> {
        if let Ok(dir) = std::env::var("SKIPPR_APPLE_SEARCH_ADS_FIXTURE_DIR") {
            if let Some(body) = load_fixture_report(&dir, stream.grain) {
                return Ok(body);
            }
        }

        let auth = self.authorization_header().await?;
        let url = report_url(stream.grain, campaign_id, ad_group_id);
        let day = date.format("%Y-%m-%d").to_string();
        let time_zone = report_time_zone(stream, self.time_zone.as_str());
        let body = serde_json::json!({
            "startTime": day,
            "endTime": day,
            "granularity": "DAILY",
            "timeZone": time_zone,
            "returnRecordsWithNoMetrics": self.return_records_with_no_metrics,
            "selector": {
                "pagination": {
                    "offset": offset,
                    "limit": limit
                }
            }
        });
        self.post_or_get_with_retry(&auth, reqwest::Method::POST, &url, Some(body))
            .await
    }

    pub async fn fetch_report_all_pages(
        &self,
        stream: &AsaStreamDef,
        date: NaiveDate,
        campaign_id: Option<i64>,
        ad_group_id: Option<i64>,
    ) -> Result<serde_json::Value, std::io::Error> {
        let mut merged_rows: Vec<serde_json::Value> = Vec::new();
        let mut pagination = OffsetPagination {
            offset: 0,
            limit: DEFAULT_PAGE_LIMIT,
        };

        let mut last_body = self
            .fetch_report_page(
                stream,
                date,
                campaign_id,
                ad_group_id,
                pagination.offset,
                pagination.limit,
            )
            .await?;
        let mut page_rows = rows_from_report_body(&last_body);
        let mut count = page_rows.len();
        merged_rows.append(&mut page_rows);
        while !pagination.should_stop(count) {
            pagination.advance(count);
            last_body = self
                .fetch_report_page(
                    stream,
                    date,
                    campaign_id,
                    ad_group_id,
                    pagination.offset,
                    pagination.limit,
                )
                .await?;
            let page_rows = rows_from_report_body(&last_body);
            count = page_rows.len();
            merged_rows.extend(page_rows);
        }

        if let Some(data) = last_body.get_mut("data") {
            if let Some(resp) = data.get_mut("reportingDataResponse") {
                resp["row"] = serde_json::Value::Array(merged_rows);
            }
            Ok(last_body)
        } else {
            Ok(serde_json::json!({
                "data": {
                    "reportingDataResponse": {
                        "row": merged_rows
                    }
                }
            }))
        }
    }

    async fn post_or_get_with_retry(
        &self,
        auth_header: &str,
        method: reqwest::Method,
        url: &str,
        json_body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, std::io::Error> {
        let mut attempt = 0u32;
        loop {
            let mut req = self
                .http
                .client
                .request(method.clone(), url)
                .header("Authorization", auth_header)
                .header("X-AP-Context", self.org_context_header());
            if let Some(body) = &json_body {
                req = req.json(body);
            }
            let response = req
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
                            "Apple Search Ads API failed after {attempt}/{} attempts: HTTP {status}",
                            self.http.config.max_attempts
                        )));
                    }
                    warn!(
                        attempt,
                        %status,
                        %url,
                        "Apple Search Ads API transient error; backing off"
                    );
                    self.http.backoff(attempt, delay).await;
                }
                skippr_plugin_shared_api_source::RetryDecision::GiveUp => {
                    let text = response.text().await.unwrap_or_default();
                    return Err(std::io::Error::other(format!(
                        "Apple Search Ads API failed: HTTP {status} {text}"
                    )));
                }
            }
        }
    }
}

fn report_url(grain: ReportGrain, campaign_id: Option<i64>, ad_group_id: Option<i64>) -> String {
    match grain {
        ReportGrain::Campaign => format!("{ASA_API_BASE}/reports/campaigns"),
        ReportGrain::AdGroup => format!(
            "{ASA_API_BASE}/reports/campaigns/{}/adgroups",
            campaign_id.expect("campaign_id required for ad group reports")
        ),
        ReportGrain::Keyword => format!(
            "{ASA_API_BASE}/reports/campaigns/{}/adgroups/{}/keywords",
            campaign_id.expect("campaign_id required for keyword reports"),
            ad_group_id.expect("ad_group_id required for keyword reports")
        ),
        ReportGrain::SearchTerm => format!(
            "{ASA_API_BASE}/reports/campaigns/{}/searchterms",
            campaign_id.expect("campaign_id required for search term reports")
        ),
    }
}

fn load_fixture_report(dir: &str, grain: ReportGrain) -> Option<serde_json::Value> {
    let file = match grain {
        ReportGrain::Campaign => "campaign_report.json",
        ReportGrain::AdGroup => "ad_group_report.json",
        ReportGrain::Keyword => "keyword_report.json",
        ReportGrain::SearchTerm => "search_term_report.json",
    };
    let path = format!("{}/{}", dir.trim_end_matches('/'), file);
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn rows_from_report_body(body: &serde_json::Value) -> Vec<serde_json::Value> {
    body.pointer("/data/reportingDataResponse/row")
        .or_else(|| body.pointer("/reportingDataResponse/row"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

fn parse_campaign_list(body: &serde_json::Value) -> Vec<CampaignRef> {
    let items = body
        .pointer("/data")
        .or_else(|| body.get("data"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    items
        .into_iter()
        .filter_map(|item| {
            let id = item.get("id").and_then(json_to_i64)?;
            let name = item
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            Some(CampaignRef { id, name })
        })
        .collect()
}

fn parse_ad_group_list(body: &serde_json::Value, campaign_id: i64) -> Vec<AdGroupRef> {
    let items = body
        .pointer("/data")
        .or_else(|| body.get("data"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    items
        .into_iter()
        .filter_map(|item| {
            let id = item.get("id").and_then(json_to_i64)?;
            let name = item
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            Some(AdGroupRef {
                campaign_id,
                id,
                name,
            })
        })
        .collect()
}

fn json_to_i64(value: &serde_json::Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().map(|n| n as i64))
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
}

pub fn report_time_zone<'a>(stream: &'a AsaStreamDef, config_time_zone: &'a str) -> &'a str {
    stream.time_zone_override.unwrap_or(config_time_zone)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streams::{ReportGrain, CURATED_STREAMS};
    use skippr_plugin_shared_api_source::RetryConfig;

    #[test]
    fn search_term_report_uses_ortz_only() {
        let st = CURATED_STREAMS
            .iter()
            .find(|s| s.grain == ReportGrain::SearchTerm)
            .unwrap();
        let campaign = CURATED_STREAMS
            .iter()
            .find(|s| s.grain == ReportGrain::Campaign)
            .unwrap();
        assert_eq!(report_time_zone(st, "UTC"), "ORTZ");
        assert_eq!(report_time_zone(campaign, "UTC"), "UTC");
    }

    #[test]
    fn search_term_report_url_includes_campaign_id() {
        let url = report_url(ReportGrain::SearchTerm, Some(99), None);
        assert!(url.contains("/campaigns/99/searchterms"));
    }

    #[tokio::test]
    async fn authorization_header_uses_static_access_token() {
        let client = AsaApiClient::new(
            RetryableHttpClient::new(RetryConfig::default()),
            "org".into(),
            "UTC".into(),
            true,
            None,
            Some("test-token".into()),
        );
        let header = client.authorization_header().await.unwrap();
        assert_eq!(header, "Bearer test-token");
    }

    #[test]
    fn rows_from_report_body_handles_empty_and_malformed() {
        assert!(rows_from_report_body(&serde_json::json!({})).is_empty());
        assert!(rows_from_report_body(&serde_json::json!({"data": {}})).is_empty());
        assert!(rows_from_report_body(&serde_json::json!({
            "data": {"reportingDataResponse": {"row": "bad"}}
        }))
        .is_empty());
    }

    #[test]
    fn rows_from_report_body_reads_fixture_shape() {
        let fixture_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        let body: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(format!("{fixture_dir}/campaign_report.json")).unwrap(),
        )
        .unwrap();
        assert!(!rows_from_report_body(&body).is_empty());
    }
}
