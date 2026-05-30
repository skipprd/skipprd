use std::collections::{HashSet, VecDeque};
use std::time::Duration;

use url::Url;

pub fn normalize_site(site: &str) -> Result<String, std::io::Error> {
    let trimmed = site.trim();
    let with_scheme = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let parsed = Url::parse(&with_scheme).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid site '{site}': {e}"),
        )
    })?;
    Ok(format!(
        "{}://{}/",
        parsed.scheme(),
        parsed.host_str().unwrap_or_default()
    ))
}

pub fn same_site(origin: &str, url: &str) -> bool {
    let Ok(base) = Url::parse(origin) else {
        return false;
    };
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    base.host_str() == parsed.host_str() && base.scheme() == parsed.scheme()
}

pub fn canonicalize_url(raw: &str, origin: &str) -> Result<String, std::io::Error> {
    let absolute = if raw.starts_with("http://") || raw.starts_with("https://") {
        raw.to_string()
    } else if raw.starts_with('/') {
        let base = Url::parse(origin).map_err(std::io::Error::other)?;
        base.join(raw)
            .map(|u| u.to_string())
            .map_err(std::io::Error::other)?
    } else {
        format!("{origin}{}", raw.trim_start_matches('/'))
    };
    let parsed = Url::parse(&absolute).map_err(std::io::Error::other)?;
    Ok(format!(
        "{}://{}{}",
        parsed.scheme(),
        parsed.host_str().unwrap_or_default(),
        parsed.path()
    ))
}

#[derive(Debug, Clone)]
pub struct FetchResult {
    pub final_url: String,
    pub status: u16,
    pub body: String,
    pub content_type: Option<String>,
    pub headers: Vec<(String, String)>,
    pub ttfb_ms: u64,
}

pub struct HttpCrawler {
    client: reqwest::Client,
    user_agent: String,
    fixture_dir: Option<String>,
}

impl HttpCrawler {
    pub fn new(user_agent: &str) -> Result<Self, std::io::Error> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .map_err(std::io::Error::other)?;
        let fixture_dir = std::env::var("SKIPPR_SEO_CRAWL_FIXTURE_DIR")
            .ok()
            .filter(|d| !d.trim().is_empty());
        Ok(Self {
            client,
            user_agent: user_agent.to_string(),
            fixture_dir,
        })
    }

    pub async fn fetch(&self, url: &str) -> Result<FetchResult, std::io::Error> {
        if let Some(dir) = &self.fixture_dir {
            if let Some(result) = load_fetch_fixture(dir, url) {
                return Ok(result);
            }
        }
        let started = std::time::Instant::now();
        let response = self
            .client
            .get(url)
            .header("User-Agent", &self.user_agent)
            .send()
            .await
            .map_err(std::io::Error::other)?;
        let status = response.status().as_u16();
        let final_url = response.url().to_string();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let headers = response
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_string()))
            .collect();
        let body = response.text().await.map_err(std::io::Error::other)?;
        Ok(FetchResult {
            final_url,
            status,
            body,
            content_type,
            headers,
            ttfb_ms: started.elapsed().as_millis() as u64,
        })
    }
}

fn load_fetch_fixture(dir: &str, url: &str) -> Option<FetchResult> {
    let slug = url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .replace(['/', '.', ':'], "_");
    let path = format!("{}/page_{slug}.html", dir.trim_end_matches('/'));
    let body = std::fs::read_to_string(&path).ok()?;
    Some(FetchResult {
        final_url: url.to_string(),
        status: 200,
        body,
        content_type: Some("text/html".into()),
        headers: Vec::new(),
        ttfb_ms: 50,
    })
}

pub async fn fetch_robots_txt(
    crawler: &HttpCrawler,
    origin: &str,
) -> Result<(u16, String), std::io::Error> {
    let url = format!("{}robots.txt", origin.trim_end_matches('/'));
    let result = crawler.fetch(&url).await?;
    Ok((result.status, result.body))
}

pub fn parse_robots_sitemaps(body: &str) -> Vec<String> {
    body.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.to_ascii_lowercase().starts_with("sitemap:") {
                line.splitn(2, ':').nth(1).map(|s| s.trim().to_string())
            } else {
                None
            }
        })
        .filter(|s| !s.is_empty())
        .collect()
}

pub async fn discover_sitemap_urls(
    crawler: &HttpCrawler,
    origin: &str,
    robots_sitemaps: &[String],
    probe_paths: &[String],
) -> Result<Vec<(String, String)>, std::io::Error> {
    let mut sitemap_files = HashSet::new();
    for sm in robots_sitemaps {
        sitemap_files.insert(sm.clone());
    }
    for path in probe_paths {
        sitemap_files.insert(format!("{}{}", origin.trim_end_matches('/'), path));
    }

    let mut page_urls = Vec::new();
    for sm_url in sitemap_files {
        if let Ok(urls) = fetch_sitemap_recursive(crawler, &sm_url).await {
            for page in urls {
                if same_site(origin, &page) {
                    page_urls.push((page, sm_url.clone()));
                }
            }
        }
    }
    Ok(page_urls)
}

async fn fetch_sitemap_recursive(
    crawler: &HttpCrawler,
    sitemap_url: &str,
) -> Result<Vec<String>, std::io::Error> {
    let result = crawler.fetch(sitemap_url).await?;
    if !result.status.to_string().starts_with('2') {
        return Ok(Vec::new());
    }
    parse_sitemap_xml(&result.body)
}

fn parse_sitemap_xml(body: &str) -> Result<Vec<String>, std::io::Error> {
    let mut out = Vec::new();
    let mut reader = quick_xml::Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(quick_xml::events::Event::Start(e)) | Ok(quick_xml::events::Event::Empty(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name == "loc" {
                    if let Ok(quick_xml::events::Event::Text(t)) =
                        reader.read_event_into(&mut buf)
                    {
                        let text = t.unescape().unwrap_or_default().to_string();
                        if text.starts_with("http://") || text.starts_with("https://") {
                            out.push(text);
                        }
                    }
                }
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Err(e) => return Err(std::io::Error::other(format!("sitemap parse: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

#[derive(Debug, Clone)]
pub struct QueueItem {
    pub url: String,
    pub depth: u32,
    pub source: &'static str,
}

pub struct BfsCrawler {
    origin: String,
    max_urls: u32,
    max_depth: u32,
    rate_delay: Duration,
}

impl BfsCrawler {
    pub fn new(origin: String, max_urls: u32, max_depth: u32, crawl_rate_per_second: f64) -> Self {
        let delay_ms = if crawl_rate_per_second > 0.0 {
            (1000.0 / crawl_rate_per_second) as u64
        } else {
            500
        };
        Self {
            origin,
            max_urls,
            max_depth,
            rate_delay: Duration::from_millis(delay_ms.max(1)),
        }
    }

    pub fn seed_queue(&self, sitemap_urls: &[(String, String)]) -> VecDeque<QueueItem> {
        let mut queue = VecDeque::new();
        queue.push_back(QueueItem {
            url: self.origin.clone(),
            depth: 0,
            source: "tld",
        });
        for (url, _) in sitemap_urls {
            if url != &self.origin {
                queue.push_back(QueueItem {
                    url: url.clone(),
                    depth: 1,
                    source: "sitemap",
                });
            }
        }
        queue
    }

    pub fn rate_delay(&self) -> Duration {
        self.rate_delay
    }

    pub fn should_enqueue(&self, depth: u32, seen_count: usize) -> bool {
        depth <= self.max_depth && seen_count < self.max_urls as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_site_adds_https_and_slash() {
        assert_eq!(normalize_site("example.com").unwrap(), "https://example.com/");
    }

    #[test]
    fn parse_robots_sitemaps_finds_directives() {
        let body = "User-agent: *\nSitemap: https://example.com/sitemap.xml\n";
        let urls = parse_robots_sitemaps(body);
        assert_eq!(urls, vec!["https://example.com/sitemap.xml"]);
    }

    #[test]
    fn parse_sitemap_xml_extracts_loc() {
        let xml = r#"<?xml version="1.0"?><urlset><url><loc>https://example.com/a</loc></url></urlset>"#;
        let urls = parse_sitemap_xml(xml).unwrap();
        assert_eq!(urls, vec!["https://example.com/a"]);
    }
}
