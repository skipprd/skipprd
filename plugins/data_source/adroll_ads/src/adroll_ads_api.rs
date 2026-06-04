use crate::streams::{AdRollStreamDef, AdRollStreamKind};
use chrono::NaiveDate;
use skippr_plugin_shared_api_source::RetryableHttpClient;
use tracing::warn;
pub const ADROLL_REST_API_BASE: &str = "https://services.adroll.com/api/v1";
pub const ADROLL_REPORTING_API: &str = "https://services.adroll.com/reporting/api/v1/query";

#[derive(Clone)]
pub struct AdRollAdsApiClient {
    pub http: RetryableHttpClient,
    pub advertiser_id: String,
    pub access_token: String,
    pub reporting_url: String,
}
impl AdRollAdsApiClient {
    pub fn new(
        http: RetryableHttpClient,
        advertiser_id: String,
        access_token: String,
        reporting_url: Option<String>,
    ) -> Self {
        Self {
            http,
            advertiser_id: advertiser_id.trim().to_string(),
            access_token,
            reporting_url: reporting_url
                .filter(|u| !u.trim().is_empty())
                .unwrap_or_else(|| ADROLL_REPORTING_API.to_string()),
        }
    }
    pub fn url_for(&self, stream: &AdRollStreamDef) -> String {
        match stream.kind {
            AdRollStreamKind::Advertisers => format!("{ADROLL_REST_API_BASE}/advertisables"),
            AdRollStreamKind::Campaigns => format!(
                "{ADROLL_REST_API_BASE}/advertisable/{}/campaigns",
                self.advertiser_id
            ),
            AdRollStreamKind::AdGroups => format!(
                "{ADROLL_REST_API_BASE}/advertisable/{}/adgroups",
                self.advertiser_id
            ),
            AdRollStreamKind::Ads => format!(
                "{ADROLL_REST_API_BASE}/advertisable/{}/ads",
                self.advertiser_id
            ),
            AdRollStreamKind::Reporting => self.reporting_url.clone(),
        }
    }
    pub fn build_request(
        &self,
        stream: &AdRollStreamDef,
        date: Option<NaiveDate>,
    ) -> reqwest::RequestBuilder {
        let mut req = self
            .http
            .client
            .request(
                if matches!(stream.kind, AdRollStreamKind::Reporting) {
                    reqwest::Method::POST
                } else {
                    reqwest::Method::GET
                },
                self.url_for(stream),
            )
            .bearer_auth(self.access_token.trim());
        if matches!(stream.kind, AdRollStreamKind::Reporting) {
            let d = date.expect("reporting date required");
            req = req.json(&serde_json::json!({"query": reporting_query(), "variables": {"advertisableEid": self.advertiser_id, "startDate": d.format("%Y-%m-%d").to_string(), "endDate": d.format("%Y-%m-%d").to_string()}}));
        }
        req
    }
    pub async fn fetch(
        &self,
        stream: &AdRollStreamDef,
        date: Option<NaiveDate>,
    ) -> Result<serde_json::Value, std::io::Error> {
        if let Ok(dir) = std::env::var("SKIPPR_ADROLL_ADS_FIXTURE_DIR") {
            if !dir.trim().is_empty() {
                if let Some(body) = load_fixture(&dir, stream.namespace) {
                    return Ok(body);
                }
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!(
                        "AdRoll fixture missing for namespace {} under {}",
                        stream.namespace, dir
                    ),
                ));
            }
        }
        let mut attempt = 0u32;
        loop {
            let response = self
                .build_request(stream, date)
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
                    let body: serde_json::Value = response
                        .json()
                        .await
                        .map_err(|e| std::io::Error::other(e.to_string()))?;
                    if let Some(errors) = body.get("errors").and_then(|v| v.as_array()) {
                        if !errors.is_empty() {
                            let msg = errors
                                .iter()
                                .filter_map(|e| e.get("message").and_then(|m| m.as_str()))
                                .collect::<Vec<_>>()
                                .join("; ");
                            return Err(std::io::Error::other(format!(
                                "AdRoll GraphQL errors: {}",
                                if msg.is_empty() { "unknown" } else { msg.as_str() }
                            )));
                        }
                    }
                    return Ok(body);
                }
                skippr_plugin_shared_api_source::RetryDecision::RetryAfter(delay) => {
                    attempt += 1;
                    if attempt >= self.http.config.max_attempts {
                        return Err(std::io::Error::other(format!(
                            "AdRoll API failed after {attempt}/{} attempts: HTTP {status}",
                            self.http.config.max_attempts
                        )));
                    }
                    warn!(attempt, %status, "AdRoll transient error; backing off");
                    self.http.backoff(attempt, delay).await;
                }
                skippr_plugin_shared_api_source::RetryDecision::GiveUp => {
                    let text = response.text().await.unwrap_or_default();
                    return Err(std::io::Error::other(format!(
                        "AdRoll API failed: HTTP {status} {text}"
                    )));
                }
            }
        }
    }
}
pub fn reporting_query() -> &'static str {
    "query SkipprAdRollReport($advertisableEid: String!, $startDate: Date!, $endDate: Date!) { advertisable(eid: $advertisableEid) { reports(startDate: $startDate, endDate: $endDate, granularity: DAY) { date campaignEid campaignName adGroupEid adGroupName adEid adName impressions clicks spend conversions revenue } } }"
}
fn load_fixture(dir: &str, namespace: &str) -> Option<serde_json::Value> {
    let file = match namespace {
        "adroll_ads.advertisers" => "advertisers.json",
        "adroll_ads.campaigns" => "campaigns.json",
        "adroll_ads.ad_groups" => "ad_groups.json",
        "adroll_ads.ads" => "ads.json",
        "adroll_ads.reporting_daily" => "reporting.json",
        _ => return None,
    };
    let bytes = std::fs::read(format!("{}/{}", dir.trim_end_matches('/'), file)).ok()?;
    serde_json::from_slice(&bytes).ok()
}
pub fn rows_from_body(
    body: &serde_json::Value,
    stream: &AdRollStreamDef,
) -> Vec<serde_json::Value> {
    if matches!(stream.kind, AdRollStreamKind::Reporting) {
        return body
            .pointer("/data/advertisable/reports")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
    }
    body.get("results")
        .or_else(|| body.get("data"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}
pub fn parse_rows(
    body: &serde_json::Value,
    stream: &AdRollStreamDef,
    advertiser_id: &str,
    date: Option<NaiveDate>,
) -> Vec<serde_json::Value> {
    rows_from_body(body, stream)
        .into_iter()
        .map(|row| {
            let mut obj = row.as_object().cloned().unwrap_or_default();
            obj.insert(
                "advertiser_id".into(),
                serde_json::Value::String(advertiser_id.to_string()),
            );
            if let Some(d) = date {
                obj.entry("date")
                    .or_insert(serde_json::Value::String(d.format("%Y-%m-%d").to_string()));
            }
            for (from, to) in [
                ("eid", id_key(stream)),
                ("campaignEid", "campaign_id"),
                ("adGroupEid", "ad_group_id"),
                ("adEid", "ad_id"),
            ] {
                if let Some(v) = obj.get(from).cloned() {
                    obj.entry(to).or_insert(v);
                }
            }
            serde_json::Value::Object(obj)
        })
        .collect()
}
fn id_key(stream: &AdRollStreamDef) -> &'static str {
    match stream.kind {
        AdRollStreamKind::Advertisers => "advertiser_id",
        AdRollStreamKind::Campaigns => "campaign_id",
        AdRollStreamKind::AdGroups => "ad_group_id",
        AdRollStreamKind::Ads => "ad_id",
        AdRollStreamKind::Reporting => "report_id",
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::streams::CURATED_STREAMS;
    use skippr_plugin_shared_api_source::{RetryConfig, RetryableHttpClient};
    #[test]
    fn reporting_uses_graphql_post() {
        let client = AdRollAdsApiClient::new(
            RetryableHttpClient::new(RetryConfig::default()),
            "adv".into(),
            "token".into(),
            None,
        );
        let req = client
            .build_request(
                &CURATED_STREAMS[4],
                Some(NaiveDate::from_ymd_opt(2024, 1, 1).unwrap()),
            )
            .build()
            .unwrap();
        assert_eq!(req.method(), reqwest::Method::POST);
        assert_eq!(req.url().as_str(), ADROLL_REPORTING_API);
    }

    #[test]
    fn fixture_reporting_parses_rows() {
        let fixture_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        std::env::set_var("SKIPPR_ADROLL_ADS_FIXTURE_DIR", fixture_dir);
        let client = AdRollAdsApiClient::new(
            RetryableHttpClient::new(RetryConfig::default()),
            "adv_test_001".into(),
            "fixture".into(),
            None,
        );
        let stream = &CURATED_STREAMS[4];
        let rt = tokio::runtime::Runtime::new().unwrap();
        let body = rt
            .block_on(client.fetch(stream, Some(NaiveDate::from_ymd_opt(2024, 1, 1).unwrap())))
            .expect("fixture reporting");
        let rows = parse_rows(
            &body,
            stream,
            "adv_test_001",
            Some(NaiveDate::from_ymd_opt(2024, 1, 1).unwrap()),
        );
        assert!(!rows.is_empty());
        assert_eq!(rows[0]["campaign_id"], "camp_001");
        std::env::remove_var("SKIPPR_ADROLL_ADS_FIXTURE_DIR");
    }
}
