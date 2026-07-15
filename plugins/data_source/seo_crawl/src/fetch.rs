use std::collections::HashMap;
use std::time::{Duration, Instant};

use skippr_plugin_shared_api_source::{RetryConfig, RetryableHttpClient};
use url::Url;

use crate::origin::{normalize_url_for_crawl, SiteOrigin};

#[derive(Clone, Debug)]
pub struct FetchResponse {
    pub final_url: String,
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: String,
    pub redirect_chain: Vec<u16>,
    pub ttfb_ms: u64,
}

pub struct HttpFetcher {
    http: RetryableHttpClient,
    user_agent: String,
    fixture_dir: Option<String>,
    max_response_bytes: usize,
}

impl HttpFetcher {
    pub fn new(user_agent: impl Into<String>) -> Self {
        Self {
            http: RetryableHttpClient::new(RetryConfig::default()),
            user_agent: user_agent.into(),
            fixture_dir: std::env::var("SKIPPR_SEO_CRAWL_FIXTURE_DIR")
                .ok()
                .filter(|d| !d.trim().is_empty()),
            max_response_bytes: 2_097_152,
        }
    }

    pub fn with_max_response_bytes(mut self, max_response_bytes: usize) -> Self {
        self.max_response_bytes = max_response_bytes.max(1024);
        self
    }

    pub fn with_fixture_dir(mut self, dir: impl Into<String>) -> Self {
        self.fixture_dir = Some(dir.into());
        self
    }

    pub async fn get(
        &self,
        url: &str,
        origin: &SiteOrigin,
    ) -> Result<FetchResponse, std::io::Error> {
        if let Some(dir) = &self.fixture_dir {
            if let Some(resp) = self.read_fixture(dir, url, origin)? {
                return Ok(resp);
            }
        }
        self.fetch_live(url).await
    }

    fn read_fixture(
        &self,
        dir: &str,
        url: &str,
        origin: &SiteOrigin,
    ) -> Result<Option<FetchResponse>, std::io::Error> {
        let base = dir.trim_end_matches('/');
        let path = fixture_path_for_url(base, url);
        if let Ok(bytes) = std::fs::read(&path) {
            let body = String::from_utf8_lossy(&bytes).into_owned();
            return Ok(Some(FetchResponse {
                final_url: url.to_string(),
                status: 200,
                headers: HashMap::from([(
                    "content-type".into(),
                    "text/html; charset=utf-8".into(),
                )]),
                body,
                redirect_chain: vec![200],
                ttfb_ms: 1,
            }));
        }
        if url.ends_with("/robots.txt") {
            let robots_path = format!("{base}/robots.txt");
            if let Ok(bytes) = std::fs::read(&robots_path) {
                let body = String::from_utf8_lossy(&bytes).into_owned();
                return Ok(Some(FetchResponse {
                    final_url: format!("{}/robots.txt", origin.origin),
                    status: 200,
                    headers: HashMap::new(),
                    body,
                    redirect_chain: vec![200],
                    ttfb_ms: 1,
                }));
            }
        }
        if url.contains("sitemap") {
            for name in ["sitemap.xml", "sitemap_index.xml"] {
                let sitemap_path = format!("{base}/{name}");
                if let Ok(bytes) = std::fs::read(&sitemap_path) {
                    let body = String::from_utf8_lossy(&bytes).into_owned();
                    return Ok(Some(FetchResponse {
                        final_url: url.to_string(),
                        status: 200,
                        headers: HashMap::new(),
                        body,
                        redirect_chain: vec![200],
                        ttfb_ms: 1,
                    }));
                }
            }
        }
        Ok(None)
    }

    async fn fetch_live(&self, url: &str) -> Result<FetchResponse, std::io::Error> {
        let started = Instant::now();
        let response = self
            .http
            .client
            .get(url)
            .header("User-Agent", &self.user_agent)
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let status = response.status().as_u16();
        let final_url = response.url().to_string();
        let headers: HashMap<String, String> = response
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_ascii_lowercase(),
                    v.to_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        let body = response
            .text()
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let body = if body.len() > self.max_response_bytes {
            let mut end = self.max_response_bytes.min(body.len());
            while end > 0 && !body.is_char_boundary(end) {
                end -= 1;
            }
            body[..end].to_string()
        } else {
            body
        };
        Ok(FetchResponse {
            final_url,
            status,
            headers,
            body,
            redirect_chain: vec![status],
            ttfb_ms: started.elapsed().as_millis() as u64,
        })
    }
}

pub fn fixture_path_for_url(fixture_base: &str, url: &str) -> String {
    use crate::origin::url_hash;
    let hash = url_hash(url);
    format!("{fixture_base}/pages/{hash}.html")
}

pub fn resolve_href(href: &str, page_url: &str, origin: &SiteOrigin) -> Option<String> {
    let base = Url::parse(page_url).ok()?;
    let joined = base.join(href.trim()).ok()?;
    normalize_url_for_crawl(joined.as_str(), origin)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::origin::normalize_site;

    #[test]
    fn fixture_path_uses_url_hash() {
        let path = fixture_path_for_url("/tmp/fix", "https://example.com/");
        assert!(path.contains("/pages/"));
        assert!(path.ends_with(".html"));
    }

    #[tokio::test]
    async fn reads_fixture_html() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        let origin = normalize_site("https://example.com").unwrap();
        let fetcher = HttpFetcher::new("test").with_fixture_dir(dir);
        let resp = fetcher.get("https://example.com/", &origin).await.unwrap();
        assert!(resp.body.contains("<html"));
    }
}
