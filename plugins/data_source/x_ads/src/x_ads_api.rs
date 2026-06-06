use chrono::NaiveDate;
use hmac::{Hmac, Mac};
use sha1::Sha1;
use skippr_plugin_shared_api_source::RetryableHttpClient;
use tracing::warn;

use crate::streams::{XAdsStreamDef, XAdsStreamKind};

type HmacSha1 = Hmac<Sha1>;
pub const X_ADS_API_BASE: &str = "https://ads-api.x.com/12";

#[derive(Clone, Debug)]
pub struct XOAuth1Credentials {
    pub consumer_key: String,
    pub consumer_secret: String,
    pub token: String,
    pub token_secret: String,
}

#[derive(Clone)]
pub struct XAdsApiClient {
    pub http: RetryableHttpClient,
    pub account_id: String,
    pub oauth: Option<XOAuth1Credentials>,
    pub bearer_token: Option<String>,
}

impl XAdsApiClient {
    pub fn new(
        http: RetryableHttpClient,
        account_id: String,
        oauth: Option<XOAuth1Credentials>,
        bearer_token: Option<String>,
    ) -> Self {
        Self {
            http,
            account_id: normalize_account_id(&account_id),
            oauth,
            bearer_token,
        }
    }
    pub fn url_for(&self, stream: &XAdsStreamDef, date: Option<NaiveDate>) -> String {
        match stream.kind {
            XAdsStreamKind::Accounts => format!("{X_ADS_API_BASE}/accounts"),
            XAdsStreamKind::Campaigns => {
                format!("{X_ADS_API_BASE}/accounts/{}/campaigns", self.account_id)
            }
            XAdsStreamKind::LineItems => {
                format!("{X_ADS_API_BASE}/accounts/{}/line_items", self.account_id)
            }
            XAdsStreamKind::PromotedPosts => format!(
                "{X_ADS_API_BASE}/accounts/{}/promoted_tweets",
                self.account_id
            ),
            XAdsStreamKind::Analytics => {
                let d = date.expect("analytics date required");
                format!("{X_ADS_API_BASE}/stats/accounts/{}?entity=LINE_ITEM&granularity=DAY&start_time={}T00:00:00Z&end_time={}T23:59:59Z&metric_groups=ENGAGEMENT,BILLING,VIDEO", self.account_id, d.format("%Y-%m-%d"), d.format("%Y-%m-%d"))
            }
        }
    }
    pub fn build_request(
        &self,
        stream: &XAdsStreamDef,
        date: Option<NaiveDate>,
    ) -> Result<reqwest::RequestBuilder, std::io::Error> {
        let url = self.url_for(stream, date);
        let mut req = self.http.client.get(&url);
        if let Some(token) = self
            .bearer_token
            .as_deref()
            .filter(|t| !t.trim().is_empty())
        {
            req = req.bearer_auth(token.trim());
        } else if let Some(oauth) = &self.oauth {
            req = req.header(
                "Authorization",
                oauth1_authorization_header("GET", &url, oauth, fixed_nonce(), fixed_timestamp()),
            );
        } else if std::env::var("SKIPPR_X_ADS_FIXTURE_DIR")
            .map(|d| !d.trim().is_empty())
            .unwrap_or(false)
        {
            req = req.header("Authorization", "OAuth fixture");
        } else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "X Ads requires OAuth 1.0a credentials or bearer token",
            ));
        }
        Ok(req)
    }
    pub async fn fetch(
        &self,
        stream: &XAdsStreamDef,
        date: Option<NaiveDate>,
    ) -> Result<serde_json::Value, std::io::Error> {
        if let Ok(dir) = std::env::var("SKIPPR_X_ADS_FIXTURE_DIR") {
            if !dir.trim().is_empty() {
                return load_fixture(&dir, stream.namespace).ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("fixture missing for {} in {}", stream.namespace, dir),
                    )
                });
            }
        }
        let mut attempt = 0u32;
        loop {
            let response = self
                .build_request(stream, date)?
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
                            "X Ads API failed after {attempt}/{} attempts: HTTP {status}",
                            self.http.config.max_attempts
                        )));
                    }
                    warn!(attempt, %status, "X Ads transient error; backing off");
                    self.http.backoff(attempt, delay).await;
                }
                skippr_plugin_shared_api_source::RetryDecision::GiveUp => {
                    let text = response.text().await.unwrap_or_default();
                    return Err(std::io::Error::other(format!(
                        "X Ads API failed: HTTP {status} {text}"
                    )));
                }
            }
        }
    }
}

pub fn normalize_account_id(account_id: &str) -> String {
    account_id
        .trim()
        .trim_start_matches("accounts/")
        .to_string()
}
fn fixed_nonce() -> String {
    std::env::var("SKIPPR_X_ADS_OAUTH_NONCE").unwrap_or_else(|_| "skipprnonce".into())
}
fn fixed_timestamp() -> String {
    std::env::var("SKIPPR_X_ADS_OAUTH_TIMESTAMP").unwrap_or_else(|_| "1700000000".into())
}

pub fn oauth1_authorization_header(
    method: &str,
    url: &str,
    creds: &XOAuth1Credentials,
    nonce: String,
    timestamp: String,
) -> String {
    let mut params = vec![
        ("oauth_consumer_key".to_string(), creds.consumer_key.clone()),
        ("oauth_nonce".to_string(), nonce),
        (
            "oauth_signature_method".to_string(),
            "HMAC-SHA1".to_string(),
        ),
        ("oauth_timestamp".to_string(), timestamp),
        ("oauth_token".to_string(), creds.token.clone()),
        ("oauth_version".to_string(), "1.0".to_string()),
    ];
    if let Some(query) = url.split_once('?').map(|(_, q)| q) {
        for pair in query.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                params.push((k.to_string(), v.to_string()));
            }
        }
    }
    params.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let param_string = params
        .iter()
        .map(|(k, v)| format!("{}={}", pct(k), pct(v)))
        .collect::<Vec<_>>()
        .join("&");
    let base_url = url.split('?').next().unwrap_or(url);
    let base = format!(
        "{}&{}&{}",
        method.to_ascii_uppercase(),
        pct(base_url),
        pct(&param_string)
    );
    let key = format!(
        "{}&{}",
        pct(&creds.consumer_secret),
        pct(&creds.token_secret)
    );
    let mut mac = HmacSha1::new_from_slice(key.as_bytes()).expect("HMAC accepts any key length");
    mac.update(base.as_bytes());
    let sig = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        mac.finalize().into_bytes(),
    );
    let oauth_params = params
        .into_iter()
        .filter(|(k, _)| k.starts_with("oauth_"))
        .map(|(k, v)| {
            if k == "oauth_signature" {
                (k, v)
            } else {
                (k, v)
            }
        })
        .collect::<Vec<_>>();
    let mut header_params = oauth_params;
    header_params.push(("oauth_signature".into(), sig));
    header_params.sort_by(|a, b| a.0.cmp(&b.0));
    format!(
        "OAuth {}",
        header_params
            .into_iter()
            .map(|(k, v)| format!("{}=\"{}\"", pct(&k), pct(&v)))
            .collect::<Vec<_>>()
            .join(", ")
    )
}
fn pct(value: &str) -> String {
    urlencoding::encode(value).replace("+", "%20")
}
fn load_fixture(dir: &str, namespace: &str) -> Option<serde_json::Value> {
    let file = fixture_file_for_namespace(namespace)?;
    let path = format!("{}/{}", dir.trim_end_matches('/'), file);
    let bytes = std::fs::read(&path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn fixture_file_for_namespace(namespace: &str) -> Option<&'static str> {
    match namespace {
        "x_ads.accounts" => Some("accounts.json"),
        "x_ads.campaigns" => Some("campaigns.json"),
        "x_ads.line_items" => Some("line_items.json"),
        "x_ads.promoted_posts" => Some("promoted_posts.json"),
        "x_ads.analytics_daily" => Some("analytics.json"),
        _ => None,
    }
}
pub fn rows_from_body(body: &serde_json::Value) -> Vec<serde_json::Value> {
    body.get("data")
        .and_then(|v| v.as_array())
        .cloned()
        .or_else(|| {
            body.get("data")
                .and_then(|v| v.get("id_data"))
                .and_then(|v| v.as_array())
                .cloned()
        })
        .unwrap_or_default()
}
pub fn parse_rows(
    body: &serde_json::Value,
    stream: &XAdsStreamDef,
    account_id: &str,
    date: Option<NaiveDate>,
) -> Vec<serde_json::Value> {
    rows_from_body(body)
        .into_iter()
        .map(|row| {
            let mut obj = row.as_object().cloned().unwrap_or_default();
            if matches!(stream.kind, XAdsStreamKind::Analytics) {
                flatten_analytics_metrics(&mut obj, &row);
            }
            obj.insert(
                "account_id".into(),
                serde_json::Value::String(normalize_account_id(account_id)),
            );
            if let Some(d) = date {
                obj.insert(
                    "date".into(),
                    serde_json::Value::String(d.format("%Y-%m-%d").to_string()),
                );
            }
            let key = match stream.kind {
                XAdsStreamKind::Campaigns => "campaign_id",
                XAdsStreamKind::LineItems => "line_item_id",
                XAdsStreamKind::PromotedPosts => "promoted_post_id",
                XAdsStreamKind::Analytics => "entity_id",
                XAdsStreamKind::Accounts => "account_id",
            };
            if !obj.contains_key(key) {
                if let Some(id) = obj
                    .get("id")
                    .cloned()
                    .or_else(|| obj.get("id_str").cloned())
                {
                    obj.insert(key.into(), id);
                }
            }
            if matches!(stream.kind, XAdsStreamKind::Analytics) {
                obj.entry("entity_type")
                    .or_insert(serde_json::Value::String("LINE_ITEM".into()));
            }
            serde_json::Value::Object(obj)
        })
        .collect()
}

fn flatten_analytics_metrics(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    row: &serde_json::Value,
) {
    let metrics = row
        .get("id_data")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|e| e.get("metrics"))
        .and_then(|m| m.as_object());
    let Some(metrics) = metrics else {
        return;
    };
    for (key, value) in metrics {
        let scalar = value
            .as_array()
            .and_then(|a| a.first())
            .cloned()
            .unwrap_or_else(|| value.clone());
        obj.entry(key.clone()).or_insert(scalar);
    }
    if let Some(micro) = obj
        .get("billed_charge_local_micro")
        .and_then(|v| v.as_f64())
    {
        obj.insert("spend".into(), serde_json::Value::from(micro / 1_000_000.0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixture_sync_produces_rows() {
        use skippr_plugin_shared_api_source::{RetryConfig, RetryableHttpClient};

        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        std::env::set_var("SKIPPR_X_ADS_FIXTURE_DIR", dir);
        let creds = XOAuth1Credentials {
            consumer_key: "ck".into(),
            consumer_secret: "cs".into(),
            token: "tk".into(),
            token_secret: "ts".into(),
        };
        let client = XAdsApiClient::new(
            RetryableHttpClient::new(RetryConfig::default()),
            "18ce54d4x5t".into(),
            Some(creds),
            None,
        );
        let rt = tokio::runtime::Runtime::new().unwrap();
        let campaign_stream = &crate::streams::CURATED_STREAMS[1];
        let campaigns = rt.block_on(client.fetch(campaign_stream, None)).unwrap();
        let rows = parse_rows(&campaigns, campaign_stream, "18ce54d4x5t", None);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].get("campaign_id").and_then(|v| v.as_str()),
            Some("camp001")
        );
        let analytics_stream = &crate::streams::CURATED_STREAMS[4];
        let sample_date = chrono::NaiveDate::from_ymd_opt(2024, 1, 15).unwrap();
        let analytics = rt
            .block_on(client.fetch(analytics_stream, Some(sample_date)))
            .unwrap();
        let arows = parse_rows(
            &analytics,
            analytics_stream,
            "18ce54d4x5t",
            Some(sample_date),
        );
        assert_eq!(arows.len(), 1);
        assert!(arows[0].get("spend").and_then(|v| v.as_f64()).unwrap() > 0.0);
        std::env::remove_var("SKIPPR_X_ADS_FIXTURE_DIR");
    }

    #[test]
    fn oauth1_header_signs_request() {
        let creds = XOAuth1Credentials {
            consumer_key: "ck".into(),
            consumer_secret: "cs".into(),
            token: "tk".into(),
            token_secret: "ts".into(),
        };
        let h = oauth1_authorization_header(
            "GET",
            "https://ads-api.x.com/12/accounts/abc/campaigns",
            &creds,
            "nonce".into(),
            "1700000000".into(),
        );
        assert!(h.contains("oauth_signature_method=\"HMAC-SHA1\""));
        assert!(h.contains("oauth_signature="));
        assert!(h.contains("oauth_consumer_key=\"ck\""));
    }
}
