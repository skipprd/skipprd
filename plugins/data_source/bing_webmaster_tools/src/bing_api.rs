use chrono::{NaiveDate, TimeZone, Utc};
use skippr_plugin_shared_api_source::RetryableHttpClient;
use tracing::warn;

use crate::streams::BingApiMethod;

pub const BING_WEBMASTER_API_BASE: &str = "https://ssl.bing.com/webmaster/api.svc/json";
pub const DEFAULT_BING_OAUTH_TOKEN_URL: &str = "https://www.bing.com/webmasters/oauth/token";

pub fn normalize_site_url(site_url: &str) -> String {
    let trimmed = site_url.trim();
    if trimmed.starts_with("sc-domain:") {
        return trimmed.to_string();
    }
    let mut url = trimmed.to_string();
    if !url.ends_with('/') {
        url.push('/');
    }
    url
}

pub fn is_forbidden_site_error(err: &std::io::Error) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("403") || msg.contains("forbidden") || msg.contains("permission")
}

pub fn api_method_name(method: BingApiMethod) -> &'static str {
    match method {
        BingApiMethod::GetRankAndTrafficStats => "GetRankAndTrafficStats",
        BingApiMethod::GetQueryStats => "GetQueryStats",
        BingApiMethod::GetPageStats => "GetPageStats",
        BingApiMethod::GetCrawlStats => "GetCrawlStats",
    }
}

pub fn fixture_slug(site_url: &str) -> String {
    site_url
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .replace(['/', ':', '.'], "_")
}

/// Parse Bing .NET JSON dates such as `/Date(1399100400000)/` or `/Date(1399100400000-0700)/`.
pub fn parse_dotnet_date(value: &str) -> Option<NaiveDate> {
    let trimmed = value.trim();
    let inner = trimmed.strip_prefix("/Date(")?.strip_suffix(")/")?;
    let millis_str = inner.split('-').next()?;
    let millis: i64 = millis_str.parse().ok()?;
    let secs = millis / 1000;
    Utc.timestamp_opt(secs, 0)
        .single()
        .map(|dt| dt.date_naive())
}

pub fn row_date(row: &serde_json::Value) -> Option<NaiveDate> {
    let date_value = row.get("Date").or_else(|| row.get("date"))?;
    if let Some(s) = date_value.as_str() {
        if let Some(parsed) = parse_dotnet_date(s) {
            return Some(parsed);
        }
        return NaiveDate::parse_from_str(s, "%Y-%m-%d").ok();
    }
    None
}

pub async fn authorization_header(
    api_key: Option<&str>,
    access_token: Option<&str>,
    oauth: Option<&skippr_plugin_shared_api_source::OAuth2RefreshTokenAuth>,
) -> Result<AuthMode, std::io::Error> {
    if std::env::var("SKIPPR_BING_WEBMASTER_TOOLS_FIXTURE_DIR")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .is_some()
    {
        return Ok(AuthMode::Fixture);
    }
    if let Some(key) = api_key.filter(|k| !k.trim().is_empty()) {
        return Ok(AuthMode::ApiKey(key.trim().to_string()));
    }
    if let Some(token) = access_token.filter(|t| !t.trim().is_empty()) {
        return Ok(AuthMode::Bearer(token.trim().to_string()));
    }
    if let Some(oauth) = oauth {
        let token = oauth.refresh().await.map_err(std::io::Error::other)?;
        return Ok(AuthMode::Bearer(token));
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "Bing Webmaster Tools requires api_key, access_token, or OAuth refresh credentials \
         (unless SKIPPR_BING_WEBMASTER_TOOLS_FIXTURE_DIR is set)",
    ))
}

#[derive(Debug, Clone)]
pub enum AuthMode {
    ApiKey(String),
    Bearer(String),
    Fixture,
}

pub async fn fetch_api_rows(
    http: &RetryableHttpClient,
    auth: &AuthMode,
    site_url: &str,
    method: BingApiMethod,
) -> Result<Vec<serde_json::Value>, std::io::Error> {
    if let AuthMode::Fixture = auth {
        return load_fixture_rows(site_url, method);
    }

    let method_name = api_method_name(method);
    let encoded_site = urlencoding::encode(site_url);
    let mut url = format!("{BING_WEBMASTER_API_BASE}/{method_name}?siteUrl={encoded_site}");

    let bearer = match auth {
        AuthMode::ApiKey(key) => {
            url.push_str(&format!("&apikey={}", urlencoding::encode(key)));
            None
        }
        AuthMode::Bearer(token) => Some(format!("Bearer {token}")),
        AuthMode::Fixture => unreachable!(),
    };

    let body = send_json_request(http, bearer.as_deref(), &url).await?;
    extract_d_array(body)
}

fn load_fixture_rows(site_url: &str, method: BingApiMethod) -> Result<Vec<serde_json::Value>, std::io::Error> {
    let fixture_dir = std::env::var("SKIPPR_BING_WEBMASTER_TOOLS_FIXTURE_DIR").map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "SKIPPR_BING_WEBMASTER_TOOLS_FIXTURE_DIR is not set",
        )
    })?;
    let method_slug = api_method_name(method).to_ascii_lowercase();
    let path = format!(
        "{}/{method_slug}_{}.json",
        fixture_dir.trim_end_matches('/'),
        fixture_slug(site_url)
    );
    let bytes = std::fs::read(&path).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("fixture not found at {path}: {e}"),
        )
    })?;
    let body: serde_json::Value = serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
    extract_d_array(body)
}

fn extract_d_array(body: serde_json::Value) -> Result<Vec<serde_json::Value>, std::io::Error> {
    let rows = body
        .get("d")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(rows)
}

pub fn bronze_rows_for_stream(
    api_rows: &[serde_json::Value],
    site_url: &str,
    dimension_fields: &[&str],
    partition_date: Option<NaiveDate>,
    start: NaiveDate,
    end: NaiveDate,
) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for row in api_rows {
        let row_date = row_date(row).or(partition_date);
        let Some(date) = row_date else {
            continue;
        };
        if date < start || date > end {
            continue;
        }
        let mut record = serde_json::Map::new();
        record.insert("site_url".into(), serde_json::Value::String(site_url.to_string()));
        record.insert(
            "date".into(),
            serde_json::Value::String(date.format("%Y-%m-%d").to_string()),
        );
        if let Some(obj) = row.as_object() {
            for (key, value) in obj {
                if key == "__type" {
                    continue;
                }
                record.insert(key.clone(), value.clone());
            }
        }
        for field in dimension_fields {
            if let Some(value) = row.get(*field) {
                record.insert(field.to_string(), value.clone());
            }
        }
        out.push(serde_json::Value::Object(record));
    }
    out
}

async fn send_json_request(
    http: &RetryableHttpClient,
    auth_header: Option<&str>,
    url: &str,
) -> Result<serde_json::Value, std::io::Error> {
    let mut attempt = 0u32;
    loop {
        let mut request = http.client.get(url);
        if let Some(header) = auth_header {
            request = request.header("Authorization", header);
        }
        let response = request
            .send()
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let status = response.status();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        match skippr_plugin_shared_api_source::RetryableHttpClient::classify_status(status, retry_after)
        {
            skippr_plugin_shared_api_source::RetryDecision::Success => {
                return response
                    .json()
                    .await
                    .map_err(|e| std::io::Error::other(e.to_string()));
            }
            skippr_plugin_shared_api_source::RetryDecision::RetryAfter(delay) => {
                attempt += 1;
                if attempt >= http.config.max_attempts {
                    return Err(std::io::Error::other(format!(
                        "Bing Webmaster request failed after {attempt}/{} attempts: HTTP {status} {url}",
                        http.config.max_attempts
                    )));
                }
                warn!(
                    attempt,
                    max_attempts = http.config.max_attempts,
                    delay_secs = delay.as_secs(),
                    %status,
                    %url,
                    "Bing Webmaster rate limited or transient error; backing off before retry"
                );
                http.backoff(attempt, delay).await;
            }
            skippr_plugin_shared_api_source::RetryDecision::GiveUp => {
                let text = response.text().await.unwrap_or_default();
                return Err(std::io::Error::other(format!(
                    "Bing Webmaster request failed: HTTP {status} {text}"
                )));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env;

    #[test]
    fn normalize_site_url_adds_trailing_slash() {
        assert_eq!(
            normalize_site_url("https://example.com"),
            "https://example.com/"
        );
    }

    #[test]
    fn parse_dotnet_date_handles_millis() {
        let date = parse_dotnet_date("/Date(1399100400000)/").unwrap();
        assert_eq!(date.format("%Y-%m-%d").to_string(), "2014-05-03");
    }

    #[test]
    fn parse_dotnet_date_handles_timezone_suffix() {
        let date = parse_dotnet_date("/Date(1399014000000-0700)/").unwrap();
        assert_eq!(date.format("%Y-%m-%d").to_string(), "2014-05-02");
    }

    #[test]
    fn normalize_site_url_preserves_sc_domain() {
        assert_eq!(
            normalize_site_url("sc-domain:example.com"),
            "sc-domain:example.com"
        );
    }

    #[test]
    fn is_forbidden_site_error_detects_403() {
        let err = std::io::Error::other("Bing Webmaster request failed: HTTP 403 Forbidden");
        assert!(is_forbidden_site_error(&err));
    }

    #[test]
    fn extract_d_array_empty_when_d_missing() {
        let rows = extract_d_array(serde_json::json!({})).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn row_date_parses_iso_string() {
        let row = serde_json::json!({ "Date": "2014-05-03" });
        assert_eq!(
            row_date(&row),
            NaiveDate::from_ymd_opt(2014, 5, 3)
        );
    }

    #[test]
    fn bronze_rows_skip_rows_without_date_when_no_partition() {
        let api_rows = vec![serde_json::json!({ "Clicks": 1, "Impressions": 2 })];
        let start = NaiveDate::from_ymd_opt(2014, 5, 3).unwrap();
        let rows = bronze_rows_for_stream(&api_rows, "https://example.com/", &[], None, start, start);
        assert!(rows.is_empty());
    }

    #[test]
    fn bronze_rows_page_snapshot_uses_partition_date() {
        let api_rows = vec![serde_json::json!({
            "Url": "https://example.com/page",
            "Clicks": 1,
            "Impressions": 2
        })];
        let partition = NaiveDate::from_ymd_opt(2014, 5, 3).unwrap();
        let rows = bronze_rows_for_stream(
            &api_rows,
            "https://example.com/",
            &["Url"],
            Some(partition),
            partition,
            partition,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["date"], "2014-05-03");
        assert_eq!(rows[0]["Url"], "https://example.com/page");
    }

    #[test]
    fn bronze_rows_exclude_out_of_window_dates() {
        let api_rows = vec![
            serde_json::json!({
                "Date": "/Date(1399100400000)/",
                "Clicks": 1
            }),
            serde_json::json!({
                "Date": "/Date(1399186800000)/",
                "Clicks": 2
            }),
        ];
        let day = NaiveDate::from_ymd_opt(2014, 5, 3).unwrap();
        let rows = bronze_rows_for_stream(&api_rows, "https://example.com/", &[], None, day, day);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["date"], "2014-05-03");
    }

    #[tokio::test]
    async fn authorization_prefers_api_key_over_access_token() {
        let _guard = test_env::lock();
        test_env::clear_fixture_dir();
        let mode = authorization_header(Some("key"), Some("token"), None)
            .await
            .unwrap();
        assert!(matches!(mode, AuthMode::ApiKey(k) if k == "key"));
    }

    #[tokio::test]
    async fn authorization_uses_access_token_when_no_api_key() {
        let _guard = test_env::lock();
        test_env::clear_fixture_dir();
        let mode = authorization_header(None, Some("bearer-token"), None)
            .await
            .unwrap();
        assert!(matches!(mode, AuthMode::Bearer(t) if t == "bearer-token"));
    }

    #[test]
    fn missing_fixture_returns_not_found() {
        let _guard = test_env::lock();
        test_env::set_fixture_dir();
        let err = load_fixture_rows(
            "https://unknown-site.example/",
            BingApiMethod::GetQueryStats,
        )
        .expect_err("missing fixture");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(err.to_string().contains("getquerystats_"));
        test_env::clear_fixture_dir();
    }

    #[test]
    fn bronze_rows_filter_by_date_window() {
        let api_rows = vec![serde_json::json!({
            "Clicks": 1,
            "Impressions": 10,
            "Date": "/Date(1399100400000)/"
        })];
        let start = NaiveDate::from_ymd_opt(2014, 5, 3).unwrap();
        let rows = bronze_rows_for_stream(
            &api_rows,
            "https://example.com/",
            &[],
            None,
            start,
            start,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["date"], "2014-05-03");
        assert_eq!(rows[0]["site_url"], "https://example.com/");
    }
}
