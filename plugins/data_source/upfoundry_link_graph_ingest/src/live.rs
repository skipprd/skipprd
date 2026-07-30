use std::time::Duration;

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use skippr_plugin_shared_api_source::{body_debug_suffix, log_api_response_issue};
use tracing::warn;

const BRIGHTDATA_API_KEY_ENV: &str = "BRIGHTDATA_API_KEY";
const BRIGHTDATA_ZONE_ENV: &str = "BRIGHTDATA_ZONE";
const DEFAULT_BRIGHTDATA_API_BASE: &str = "https://api.brightdata.com";
const DEFAULT_BRIGHTDATA_ZONE: &str = "serp_api1";

#[derive(Debug, Clone)]
pub struct LiveFetchConfig {
    pub live_crawl_enabled: bool,
    pub brightdata_proxy_escalation_enabled: bool,
}

#[derive(Debug, Clone)]
pub struct LiveHtml {
    pub html: String,
    pub http_status: u16,
    pub content_mime_type: String,
    pub discovered_by: String,
}

pub async fn fetch_live_html(page_url: &str, config: &LiveFetchConfig) -> Result<LiveHtml, String> {
    if !config.live_crawl_enabled {
        return Err("live_crawl_disabled".into());
    }
    match fetch_direct_html(page_url).await {
        Ok(fetched) => Ok(LiveHtml {
            html: fetched.html,
            http_status: fetched.http_status,
            content_mime_type: fetched.content_mime_type,
            discovered_by: "live_crawl".into(),
        }),
        Err(reason) if should_escalate(&reason) && config.brightdata_proxy_escalation_enabled => {
            fetch_brightdata_html(page_url)
                .await
                .map(|fetched| LiveHtml {
                    html: fetched.html,
                    http_status: fetched.http_status,
                    content_mime_type: fetched.content_mime_type,
                    discovered_by: "brightdata_live_crawl".into(),
                })
        }
        Err(reason) => Err(reason),
    }
}

#[derive(Debug, Clone)]
struct FetchedHtml {
    html: String,
    http_status: u16,
    content_mime_type: String,
}

async fn fetch_direct_html(page_url: &str) -> Result<FetchedHtml, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .connect_timeout(Duration::from_secs(5))
        .build()
        .map_err(|err| err.to_string())?;
    let resp = client
        .get(page_url)
        .header("Accept", "text/html,application/xhtml+xml")
        .send()
        .await
        .map_err(|err| format!("live_fetch_error: {err}"))?;
    let status = resp.status();
    if status.as_u16() == 429 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        return Err("live_fetch_429".into());
    }
    if status.as_u16() == 403 {
        return Err("live_fetch_403".into());
    }
    if !status.is_success() {
        return Err(format!("live_fetch_status_{status}"));
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !content_type.is_empty() && !content_type.contains("html") {
        return Err("live_fetch_non_html".into());
    }
    let bytes = resp.bytes().await.map_err(|err| err.to_string())?;
    if bytes.len() > 2 * 1024 * 1024 {
        return Err("live_fetch_too_large".into());
    }
    let html = String::from_utf8_lossy(&bytes).to_string();
    if is_blank_or_blocked(&html) {
        return Err("live_fetch_blank_or_blocked".into());
    }
    Ok(FetchedHtml {
        html,
        http_status: status.as_u16(),
        content_mime_type: content_type,
    })
}

async fn fetch_brightdata_html(page_url: &str) -> Result<FetchedHtml, String> {
    let api_key = std::env::var(BRIGHTDATA_API_KEY_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| "brightdata_api_key_missing".to_string())?;
    let zone = std::env::var(BRIGHTDATA_ZONE_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_BRIGHTDATA_ZONE.to_string());
    let api_base = std::env::var("BRIGHTDATA_API_BASE")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_BRIGHTDATA_API_BASE.to_string());
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .connect_timeout(Duration::from_secs(30))
        .http1_only()
        .build()
        .map_err(|err| err.to_string())?;
    let body = serde_json::json!({
        "zone": zone,
        "url": page_url,
        "format": "raw",
    });
    let endpoint = format!("{}/request", api_base.trim_end_matches('/'));
    let resp = client
        .post(&endpoint)
        .header(AUTHORIZATION, format!("Bearer {api_key}"))
        .header(CONTENT_TYPE, "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|err| format!("brightdata_fetch_error: {err}"))?;
    let status = resp.status();
    let html = resp.text().await.map_err(|err| err.to_string())?;
    if !status.is_success() {
        log_api_response_issue(
            "bright_data",
            &endpoint,
            "link_graph_html",
            Some(status.as_u16()),
            &html,
        );
        return Err(format!(
            "brightdata_fetch_status_{status} ({})",
            body_debug_suffix(&html, 300)
        ));
    }
    if html.trim().is_empty() {
        log_api_response_issue(
            "bright_data",
            &endpoint,
            "link_graph_empty",
            Some(status.as_u16()),
            &html,
        );
        return Err(format!(
            "brightdata_fetch_empty ({})",
            body_debug_suffix(&html, 300)
        ));
    }
    if is_blank_or_blocked(&html) {
        warn!(
            provider = "bright_data",
            endpoint = %endpoint,
            page_url = %page_url,
            body_len = html.len(),
            "External API response issue: brightdata_blank_or_blocked"
        );
        return Err("brightdata_blank_or_blocked".into());
    }
    Ok(FetchedHtml {
        html,
        http_status: status.as_u16(),
        content_mime_type: "text/html".to_string(),
    })
}

fn should_escalate(reason: &str) -> bool {
    matches!(
        reason,
        "live_fetch_403" | "live_fetch_429" | "live_fetch_blank_or_blocked"
    )
}

fn is_blank_or_blocked(html: &str) -> bool {
    let normalized = html.to_ascii_lowercase();
    normalized.trim().len() < 128
        || normalized.contains("captcha")
        || normalized.contains("access denied")
        || normalized.contains("enable javascript")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn disabled_live_crawl_fails_closed() {
        let result = fetch_live_html(
            "https://example.com/",
            &LiveFetchConfig {
                live_crawl_enabled: false,
                brightdata_proxy_escalation_enabled: false,
            },
        )
        .await;
        assert_eq!(result.unwrap_err(), "live_crawl_disabled");
    }

    #[test]
    fn escalation_is_limited_to_blocking_signals() {
        assert!(should_escalate("live_fetch_403"));
        assert!(should_escalate("live_fetch_429"));
        assert!(should_escalate("live_fetch_blank_or_blocked"));
        assert!(!should_escalate("live_fetch_non_html"));
    }
}
