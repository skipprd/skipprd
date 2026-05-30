use skippr_plugin_shared_api_source::{RetryConfig, RetryableHttpClient};
use tracing::warn;

use crate::config::Strategy;

pub const PAGESPEED_API_BASE: &str =
    "https://pagespeedonline.googleapis.com/pagespeedonline/v5/runPagespeed";

#[derive(Clone)]
pub struct PageSpeedClient {
    pub http: RetryableHttpClient,
    api_key: String,
    categories: Vec<String>,
    locale: String,
    fixture_dir: Option<String>,
}

impl PageSpeedClient {
    pub fn new(
        api_key: String,
        categories: Vec<String>,
        locale: String,
    ) -> Self {
        let fixture_dir = std::env::var("SKIPPR_GOOGLE_PAGESPEED_FIXTURE_DIR")
            .ok()
            .filter(|d| !d.trim().is_empty());
        Self {
            http: RetryableHttpClient::new(RetryConfig::default()),
            api_key,
            categories,
            locale,
            fixture_dir,
        }
    }

    pub async fn run_pagespeed(
        &self,
        url: &str,
        strategy: &Strategy,
    ) -> Result<serde_json::Value, std::io::Error> {
        if let Some(dir) = &self.fixture_dir {
            return load_fixture(dir, url, strategy);
        }

        let mut attempt = 0u32;
        loop {
            let response = self
                .http
                .client
                .get(PAGESPEED_API_BASE)
                .query(&[
                    ("url", url),
                    ("key", self.api_key.as_str()),
                    ("strategy", strategy.as_api_str()),
                    ("locale", self.locale.as_str()),
                ])
                .query(
                    &self
                        .categories
                        .iter()
                        .map(|c| ("category", c.as_str()))
                        .collect::<Vec<_>>(),
                )
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
                            "PageSpeed API failed after {attempt} attempts: HTTP {status}"
                        )));
                    }
                    warn!(
                        url,
                        strategy = strategy.as_api_str(),
                        attempt,
                        "PageSpeed API retry"
                    );
                    self.http.backoff(attempt, delay).await;
                }
                skippr_plugin_shared_api_source::RetryDecision::GiveUp => {
                    let body = response.text().await.unwrap_or_default();
                    return Err(std::io::Error::other(format!(
                        "PageSpeed API HTTP {status}: {body}"
                    )));
                }
            }
        }
    }
}

fn load_fixture(
    dir: &str,
    url: &str,
    strategy: &Strategy,
) -> Result<serde_json::Value, std::io::Error> {
    let base = dir.trim_end_matches('/');
    let path = if url.contains("no-field") {
        format!("{base}/run_pagespeed_no_field.json")
    } else {
        match strategy {
            Strategy::Mobile => format!("{base}/run_pagespeed_mobile.json"),
            Strategy::Desktop => format!("{base}/run_pagespeed_desktop.json"),
        }
    };
    let bytes = std::fs::read(&path).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("fixture {path}: {e}"),
        )
    })?;
    serde_json::from_slice(&bytes).map_err(|e| std::io::Error::other(e.to_string()))
}
