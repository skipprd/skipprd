use chrono::NaiveDate;
use skippr_plugin_shared_api_source::RetryableHttpClient;
use tracing::warn;

const GA4_RUN_REPORT_URL: &str = "https://analyticsdata.googleapis.com/v1beta";
const GA4_SCOPE: &str = "https://www.googleapis.com/auth/analytics.readonly";

pub fn normalize_property_id(property_id: &str) -> String {
    property_id
        .trim()
        .trim_start_matches("properties/")
        .to_string()
}

/// True when the property does not support the requested dimensions/metrics (ecommerce, ads, etc.).
pub fn is_invalid_dimension_metric_error(err: &std::io::Error) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    (msg.contains("400") || msg.contains("invalid"))
        && (msg.contains("dimension")
            || msg.contains("metric")
            || msg.contains("invalid_argument")
            || msg.contains("not found"))
}

pub async fn run_report(
    http: &RetryableHttpClient,
    auth_header: &str,
    property_id: &str,
    date: NaiveDate,
    dimensions: &[&str],
    metrics: &[&str],
    keep_empty_rows: bool,
) -> Result<serde_json::Value, std::io::Error> {
    run_report_range(
        http,
        auth_header,
        property_id,
        date,
        date,
        dimensions,
        metrics,
        keep_empty_rows,
    )
    .await
}

pub async fn run_report_range(
    http: &RetryableHttpClient,
    auth_header: &str,
    property_id: &str,
    start_date: NaiveDate,
    end_date: NaiveDate,
    dimensions: &[&str],
    metrics: &[&str],
    keep_empty_rows: bool,
) -> Result<serde_json::Value, std::io::Error> {
    let property = normalize_property_id(property_id);
    let url = format!("{GA4_RUN_REPORT_URL}/properties/{property}:runReport");
    let start_str = start_date.format("%Y-%m-%d").to_string();
    let end_str = end_date.format("%Y-%m-%d").to_string();
    let body = serde_json::json!({
        "dateRanges": [{"startDate": start_str, "endDate": end_str}],
        "dimensions": dimensions.iter().map(|name| serde_json::json!({"name": name})).collect::<Vec<_>>(),
        "metrics": metrics.iter().map(|name| serde_json::json!({"name": name})).collect::<Vec<_>>(),
        "keepEmptyRows": keep_empty_rows,
    });
    let mut attempt = 0u32;
    loop {
        let response = http
            .client
            .post(&url)
            .header("Authorization", auth_header)
            .json(&body)
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
                if attempt >= http.config.max_attempts {
                    return Err(std::io::Error::other(format!(
                        "GA4 runReport failed after {attempt}/{} attempts: HTTP {status}",
                        http.config.max_attempts
                    )));
                }
                warn!(
                    attempt,
                    max_attempts = http.config.max_attempts,
                    delay_secs = delay.as_secs(),
                    %status,
                    property,
                    start = %start_str,
                    end = %end_str,
                    "GA4 runReport rate limited or transient error; backing off before retry"
                );
                http.backoff(attempt, delay).await;
            }
            skippr_plugin_shared_api_source::RetryDecision::GiveUp => {
                let text = response.text().await.unwrap_or_default();
                return Err(std::io::Error::other(format!(
                    "GA4 runReport failed: HTTP {status} {text}"
                )));
            }
        }
    }
}

pub fn ga4_analytics_scope() -> &'static str {
    GA4_SCOPE
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
            ga4_analytics_scope(),
        )
        .map_err(std::io::Error::other)?;
        let token = sa.access_token().await.map_err(std::io::Error::other)?;
        return Ok(format!("Bearer {token}"));
    }
    if let Ok(path) = std::env::var("GOOGLE_APPLICATION_CREDENTIALS") {
        if !path.trim().is_empty() {
            let sa = skippr_plugin_shared_api_source::ServiceAccountAuth::from_json_path(
                &path,
                ga4_analytics_scope(),
            )
            .map_err(std::io::Error::other)?;
            let token = sa.access_token().await.map_err(std::io::Error::other)?;
            return Ok(format!("Bearer {token}"));
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "GA4 requires access_token, OAuth refresh credentials, service_account_json_path, \
         or GOOGLE_APPLICATION_CREDENTIALS (unless SKIPPR_GA4_FIXTURE_DIR is set)",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn authorization_header_uses_static_access_token() {
        let header = authorization_header(Some("test-token"), None, None)
            .await
            .unwrap();
        assert_eq!(header, "Bearer test-token");
    }

    #[tokio::test]
    async fn authorization_header_rejects_missing_credentials() {
        let err = authorization_header(None, None, None).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("GA4 requires"));
    }

    #[test]
    fn ga4_analytics_scope_is_readonly() {
        assert!(ga4_analytics_scope().contains("analytics.readonly"));
    }

    #[test]
    fn invalid_dimension_metric_error_detects_400() {
        let err = std::io::Error::other(
            "GA4 runReport failed: HTTP 400 Bad Request INVALID_ARGUMENT: metric foo",
        );
        assert!(is_invalid_dimension_metric_error(&err));
    }

    #[test]
    fn invalid_dimension_metric_error_rejects_auth_failure() {
        let err = std::io::Error::other("GA4 runReport failed: HTTP 401 Unauthorized");
        assert!(!is_invalid_dimension_metric_error(&err));
    }
}
