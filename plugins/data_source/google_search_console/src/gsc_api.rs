use chrono::NaiveDate;
use skippr_plugin_shared_api_source::RetryableHttpClient;
use tracing::warn;

const WEBMASTERS_API_BASE: &str = "https://www.googleapis.com/webmasters/v3";
const URL_INSPECTION_API: &str = "https://searchconsole.googleapis.com/v1/urlInspection/index:inspect";
const GSC_SCOPE: &str = "https://www.googleapis.com/auth/webmasters.readonly";

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

pub fn encode_site_url_path(site_url: &str) -> String {
    urlencoding::encode(site_url).into_owned()
}

pub fn gsc_webmasters_scope() -> &'static str {
    GSC_SCOPE
}

/// True when the property does not support the requested dimension combo (e.g. searchAppearance).
pub fn is_optional_stream_error(err: &std::io::Error) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    (msg.contains("400") || msg.contains("invalid"))
        && (msg.contains("dimension")
            || msg.contains("searchappearance")
            || msg.contains("invalid_argument")
            || msg.contains("not supported"))
}

pub fn is_forbidden_site_error(err: &std::io::Error) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("403") || msg.contains("forbidden") || msg.contains("permission")
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct SearchAnalyticsRow {
    pub keys: Vec<String>,
    #[serde(default)]
    pub clicks: f64,
    #[serde(default)]
    pub impressions: f64,
    #[serde(default)]
    pub ctr: f64,
    #[serde(default)]
    pub position: f64,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct SearchAnalyticsResponse {
    #[serde(default)]
    rows: Vec<SearchAnalyticsRow>,
}

pub async fn authorization_header(
    access_token: Option<&str>,
    oauth: Option<&skippr_plugin_shared_api_source::OAuth2RefreshTokenAuth>,
    service_account_path: Option<&str>,
) -> Result<String, std::io::Error> {
    if let Some(token) = access_token.filter(|t| !t.trim().is_empty()) {
        return Ok(format!("Bearer {}", token.trim()));
    }
    if let Some(oauth) = oauth {
        let token = oauth.refresh().await.map_err(std::io::Error::other)?;
        return Ok(format!("Bearer {token}"));
    }
    if let Some(path) = service_account_path {
        let sa = skippr_plugin_shared_api_source::ServiceAccountAuth::from_json_path(
            path,
            gsc_webmasters_scope(),
        )
        .map_err(std::io::Error::other)?;
        let token = sa.access_token().await.map_err(std::io::Error::other)?;
        return Ok(format!("Bearer {token}"));
    }
    if let Ok(path) = std::env::var("GOOGLE_APPLICATION_CREDENTIALS") {
        if !path.trim().is_empty() {
            let sa = skippr_plugin_shared_api_source::ServiceAccountAuth::from_json_path(
                &path,
                gsc_webmasters_scope(),
            )
            .map_err(std::io::Error::other)?;
            let token = sa.access_token().await.map_err(std::io::Error::other)?;
            return Ok(format!("Bearer {token}"));
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "Google Search Console requires access_token, OAuth refresh credentials, \
         service_account_json_path, or GOOGLE_APPLICATION_CREDENTIALS (unless \
         SKIPPR_GOOGLE_SEARCH_CONSOLE_FIXTURE_DIR is set)",
    ))
}

pub async fn list_sites(
    http: &RetryableHttpClient,
    auth_header: &str,
) -> Result<Vec<String>, std::io::Error> {
    let url = format!("{WEBMASTERS_API_BASE}/sites");
    let body = send_json_request(http, auth_header, reqwest::Method::GET, &url, None).await?;
    let entries = body
        .get("siteEntry")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(entries
        .iter()
        .filter_map(|entry| entry.get("siteUrl").and_then(|v| v.as_str()).map(str::to_string))
        .collect())
}

pub async fn validate_site_access(
    http: &RetryableHttpClient,
    auth_header: &str,
    site_url: &str,
) -> Result<(), std::io::Error> {
    let sites = list_sites(http, auth_header).await?;
    let normalized = normalize_site_url(site_url);
    if sites.iter().any(|s| normalize_site_url(s) == normalized) {
        return Ok(());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!(
            "Google Search Console account does not have access to site_url '{normalized}'. \
             sites.list returned {} propert(ies). Grant the authenticated user or service \
             account access in Search Console.",
            sites.len()
        ),
    ))
}

pub async fn search_analytics_query_all(
    http: &RetryableHttpClient,
    auth_header: &str,
    site_url: &str,
    start_date: NaiveDate,
    end_date: NaiveDate,
    dimensions: &[&str],
    search_type: &str,
    data_state: &str,
    row_limit: u32,
) -> Result<Vec<SearchAnalyticsRow>, std::io::Error> {
    let mut all_rows = Vec::new();
    let mut start_row = 0u32;
    loop {
        let page = search_analytics_query_page(
            http,
            auth_header,
            site_url,
            start_date,
            end_date,
            dimensions,
            search_type,
            data_state,
            row_limit,
            start_row,
        )
        .await?;
        let page_len = page.len();
        all_rows.extend(page);
        if page_len == 0 || page_len < row_limit as usize {
            break;
        }
        start_row = start_row.saturating_add(row_limit);
    }
    Ok(all_rows)
}

pub async fn search_analytics_query_page(
    http: &RetryableHttpClient,
    auth_header: &str,
    site_url: &str,
    start_date: NaiveDate,
    end_date: NaiveDate,
    dimensions: &[&str],
    search_type: &str,
    data_state: &str,
    row_limit: u32,
    start_row: u32,
) -> Result<Vec<SearchAnalyticsRow>, std::io::Error> {
    if let Ok(fixture_dir) = std::env::var("SKIPPR_GOOGLE_SEARCH_CONSOLE_FIXTURE_DIR") {
        return load_search_analytics_fixture(
            &fixture_dir,
            site_url,
            start_date,
            end_date,
            dimensions,
            start_row,
            row_limit,
        );
    }

    let encoded = encode_site_url_path(site_url);
    let url = format!("{WEBMASTERS_API_BASE}/sites/{encoded}/searchAnalytics/query");
    let body = serde_json::json!({
        "startDate": start_date.format("%Y-%m-%d").to_string(),
        "endDate": end_date.format("%Y-%m-%d").to_string(),
        "dimensions": dimensions,
        "rowLimit": row_limit,
        "startRow": start_row,
        "dataState": data_state,
        "type": search_type,
    });
    let response = send_json_request(
        http,
        auth_header,
        reqwest::Method::POST,
        &url,
        Some(body),
    )
    .await?;
    let parsed: SearchAnalyticsResponse = serde_json::from_value(response)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(parsed.rows)
}

fn load_search_analytics_fixture(
    fixture_dir: &str,
    site_url: &str,
    start_date: NaiveDate,
    end_date: NaiveDate,
    dimensions: &[&str],
    start_row: u32,
    row_limit: u32,
) -> Result<Vec<SearchAnalyticsRow>, std::io::Error> {
    let dim_key = dimensions.join("_");
    let path = format!(
        "{}/search_analytics_{}_{}_{}_{}.json",
        fixture_dir.trim_end_matches('/'),
        fixture_slug(site_url),
        dim_key,
        start_date.format("%Y%m%d"),
        end_date.format("%Y%m%d"),
    );
    let bytes = std::fs::read(&path).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("fixture not found at {path}: {e}"),
        )
    })?;
    let body: SearchAnalyticsResponse = serde_json::from_slice(&bytes)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let start = start_row as usize;
    let end = start.saturating_add(row_limit as usize).min(body.rows.len());
    Ok(body.rows[start..end].to_vec())
}

fn fixture_slug(site_url: &str) -> String {
    site_url
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .replace(['/', ':', '.'], "_")
}

pub async fn list_sitemaps(
    http: &RetryableHttpClient,
    auth_header: &str,
    site_url: &str,
) -> Result<serde_json::Value, std::io::Error> {
    if let Ok(fixture_dir) = std::env::var("SKIPPR_GOOGLE_SEARCH_CONSOLE_FIXTURE_DIR") {
        let path = format!(
            "{}/sitemaps_{}.json",
            fixture_dir.trim_end_matches('/'),
            fixture_slug(site_url)
        );
        let bytes = std::fs::read(&path).map_err(std::io::Error::other)?;
        return serde_json::from_slice(&bytes).map_err(std::io::Error::other);
    }
    let encoded = encode_site_url_path(site_url);
    let url = format!("{WEBMASTERS_API_BASE}/sites/{encoded}/sitemaps");
    send_json_request(http, auth_header, reqwest::Method::GET, &url, None).await
}

pub async fn inspect_url(
    http: &RetryableHttpClient,
    auth_header: &str,
    site_url: &str,
    inspection_url: &str,
) -> Result<serde_json::Value, std::io::Error> {
    if let Ok(fixture_dir) = std::env::var("SKIPPR_GOOGLE_SEARCH_CONSOLE_FIXTURE_DIR") {
        let slug = fixture_slug(inspection_url);
        let path = format!(
            "{}/url_inspection_{}.json",
            fixture_dir.trim_end_matches('/'),
            slug
        );
        let bytes = std::fs::read(&path).map_err(std::io::Error::other)?;
        return serde_json::from_slice(&bytes).map_err(std::io::Error::other);
    }
    let body = serde_json::json!({
        "inspectionUrl": inspection_url,
        "siteUrl": site_url,
    });
    send_json_request(
        http,
        auth_header,
        reqwest::Method::POST,
        URL_INSPECTION_API,
        Some(body),
    )
    .await
}

async fn send_json_request(
    http: &RetryableHttpClient,
    auth_header: &str,
    method: reqwest::Method,
    url: &str,
    body: Option<serde_json::Value>,
) -> Result<serde_json::Value, std::io::Error> {
    let mut attempt = 0u32;
    loop {
        let mut request = http.client.request(method.clone(), url);
        request = request.header("Authorization", auth_header);
        if let Some(ref json_body) = body {
            request = request.json(json_body);
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
        match RetryableHttpClient::classify_status(status, retry_after)
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
                        "GSC request failed after {attempt}/{} attempts: HTTP {status} {url}",
                        http.config.max_attempts
                    )));
                }
                warn!(
                    attempt,
                    max_attempts = http.config.max_attempts,
                    delay_secs = delay.as_secs(),
                    %status,
                    %url,
                    "GSC rate limited or transient error; backing off before retry"
                );
                http.backoff(attempt, delay).await;
            }
            skippr_plugin_shared_api_source::RetryDecision::GiveUp => {
                let text = response.text().await.unwrap_or_default();
                return Err(std::io::Error::other(format!(
                    "GSC request failed: HTTP {status} {text}"
                )));
            }
        }
    }
}

pub fn parse_query_response_rows(
    api_rows: &[SearchAnalyticsRow],
    stream_dimensions: &[&str],
    site_url: &str,
    search_type: &str,
) -> Vec<serde_json::Value> {
    let mut out = Vec::with_capacity(api_rows.len());
    for row in api_rows {
        let mut record = serde_json::Map::new();
        record.insert("site_url".into(), serde_json::Value::String(site_url.to_string()));
        record.insert(
            "search_type".into(),
            serde_json::Value::String(search_type.to_string()),
        );
        for (idx, dim_name) in stream_dimensions.iter().enumerate() {
            let value = row.keys.get(idx).map(String::as_str).unwrap_or_default();
            record.insert((*dim_name).into(), serde_json::Value::String(value.to_string()));
        }
        record.insert("clicks".into(), serde_json::json!(row.clicks));
        record.insert("impressions".into(), serde_json::json!(row.impressions));
        record.insert("ctr".into(), serde_json::json!(row.ctr));
        record.insert("position".into(), serde_json::json!(row.position));
        out.push(serde_json::Value::Object(record));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_site_url_adds_trailing_slash_for_url_prefix() {
        assert_eq!(
            normalize_site_url("https://example.com"),
            "https://example.com/"
        );
        assert_eq!(
            normalize_site_url("sc-domain:example.com"),
            "sc-domain:example.com"
        );
    }

    #[test]
    fn encode_site_url_path_percent_encodes() {
        let encoded = encode_site_url_path("https://example.com/");
        assert!(encoded.contains("%3A"));
        assert!(encoded.contains("%2F"));
    }

    #[test]
    fn parse_query_response_rows_maps_keys_to_dimensions() {
        let rows = vec![SearchAnalyticsRow {
            keys: vec!["2024-06-01".into(), "example query".into()],
            clicks: 10.0,
            impressions: 100.0,
            ctr: 0.1,
            position: 5.2,
        }];
        let parsed = parse_query_response_rows(
            &rows,
            &["date", "query"],
            "https://example.com/",
            "web",
        );
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["date"], "2024-06-01");
        assert_eq!(parsed[0]["query"], "example query");
        assert_eq!(parsed[0]["clicks"], 10);
    }

    #[test]
    fn pagination_start_row_advances_when_page_is_full() {
        let row_limit = 2u32;
        let page1 = vec![
            SearchAnalyticsRow {
                keys: vec!["2024-01-01".into()],
                clicks: 1.0,
                impressions: 1.0,
                ctr: 0.0,
                position: 1.0,
            },
            SearchAnalyticsRow {
                keys: vec!["2024-01-02".into()],
                clicks: 2.0,
                impressions: 2.0,
                ctr: 0.0,
                position: 2.0,
            },
        ];
        assert_eq!(page1.len(), row_limit as usize);
        let mut start_row = 0u32;
        start_row = start_row.saturating_add(row_limit);
        assert_eq!(start_row, 2);
    }

    #[tokio::test]
    async fn authorization_header_uses_static_access_token() {
        let header = authorization_header(Some("test-token"), None, None)
            .await
            .unwrap();
        assert_eq!(header, "Bearer test-token");
    }

    #[test]
    fn optional_stream_error_detects_400() {
        let err = std::io::Error::other(
            "GSC request failed: HTTP 400 Bad Request INVALID_ARGUMENT: dimension searchAppearance",
        );
        assert!(is_optional_stream_error(&err));
    }

    #[test]
    fn forbidden_site_error_detects_403() {
        let err = std::io::Error::other("GSC request failed: HTTP 403 Forbidden");
        assert!(is_forbidden_site_error(&err));
    }
}
