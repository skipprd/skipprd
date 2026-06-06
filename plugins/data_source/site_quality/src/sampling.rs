use std::collections::{HashSet, VecDeque};
use std::sync::LazyLock;

use regex::Regex;
use serde::Deserialize;
use serde_derive::Serialize;
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UrlMode {
    TldSample,
    UrlList,
    /// Discover URLs by following same-origin links (BFS), then sitemap fallbacks.
    SiteCrawl,
}

#[derive(Debug, Clone)]
pub struct CandidateUrl {
    pub url: String,
    pub priority: f64,
    pub lastmod: Option<String>,
    pub depth: usize,
}

pub fn normalize_site_origin(site: &str) -> Result<String, std::io::Error> {
    let trimmed = site.trim();
    let with_scheme = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let parsed = Url::parse(&with_scheme).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid site URL '{site}': {e}"),
        )
    })?;
    let host = parsed.host_str().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("site URL missing host: {site}"),
        )
    })?;
    Ok(format!("https://{host}"))
}

pub fn homepage_url(origin: &str) -> String {
    format!("{}/", origin.trim_end_matches('/'))
}

pub fn resolve_url_list(
    origin: &str,
    url_mode: UrlMode,
    url_list: &[String],
    max_pages: u32,
    fetcher: &dyn SitemapFetcher,
    respect_robots: bool,
) -> Result<Vec<String>, std::io::Error> {
    let cap = max_pages.max(1) as usize;
    match url_mode {
        UrlMode::UrlList => {
            let mut out: Vec<String> = url_list
                .iter()
                .map(|u| canonicalize_page_url(origin, u))
                .collect::<Result<Vec<_>, _>>()?;
            out.sort();
            out.dedup();
            out.truncate(cap);
            Ok(out)
        }
        UrlMode::TldSample => {
            let home = homepage_url(origin);
            let mut candidates = vec![CandidateUrl {
                url: home.clone(),
                priority: 1.0,
                lastmod: None,
                depth: 0,
            }];
            if let Ok(robots) = fetcher.fetch_text(&format!("{origin}/robots.txt")) {
                for sitemap in parse_robots_sitemaps(&robots) {
                    candidates.extend(fetch_sitemap_urls(
                        fetcher,
                        &sitemap,
                        origin,
                        respect_robots,
                    )?);
                }
            }
            for fallback in ["/sitemap.xml", "/sitemap_index.xml"] {
                let loc = format!("{origin}{fallback}");
                if candidates.len() >= cap * 4 {
                    break;
                }
                candidates.extend(fetch_sitemap_urls(fetcher, &loc, origin, respect_robots)?);
            }
            Ok(rank_and_cap_candidates(origin, &home, candidates, cap))
        }
        UrlMode::SiteCrawl => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "site_crawl url_mode requires async resolution",
        )),
    }
}

/// Same-origin link crawl (shared with SeoCrawl plugin).
pub async fn resolve_site_crawl_urls(
    site: &str,
    max_pages: u32,
    max_depth: u32,
    seed_urls: &[String],
    respect_robots: bool,
) -> Result<Vec<String>, std::io::Error> {
    use skippr_plugin_data_source_seo_crawl::crawler::{crawl_site, seed_origin};
    use skippr_plugin_data_source_seo_crawl::fetch::HttpFetcher;

    let origin = seed_origin(site)?;
    let user_agent = "SkipprSiteQuality/1.0";
    let mut fetcher = HttpFetcher::new(user_agent);
    if let Ok(dir) = std::env::var("SKIPPR_SITE_QUALITY_FIXTURE_DIR")
        .or_else(|_| std::env::var("SKIPPR_SEO_CRAWL_FIXTURE_DIR"))
    {
        if !dir.trim().is_empty() {
            fetcher = fetcher.with_fixture_dir(dir.trim());
        }
    }
    let pages = crawl_site(
        &origin,
        &fetcher,
        user_agent,
        max_pages,
        max_depth,
        respect_robots,
        seed_urls,
    )
    .await?;
    Ok(pages.into_iter().map(|p| p.url).collect())
}

pub fn rank_and_cap_candidates(
    origin: &str,
    homepage: &str,
    mut candidates: Vec<CandidateUrl>,
    cap: usize,
) -> Vec<String> {
    for c in &mut candidates {
        if c.url == homepage {
            c.priority += 10.0;
        }
        if c.depth <= 1 {
            c.priority += 2.0;
        } else if c.depth <= 2 {
            c.priority += 1.0;
        }
    }
    candidates.sort_by(|a, b| {
        b.priority
            .partial_cmp(&a.priority)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.url.cmp(&b.url))
    });
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for c in candidates {
        if !c.url.starts_with(origin) {
            continue;
        }
        if seen.insert(c.url.clone()) {
            out.push(c.url);
        }
        if out.len() >= cap {
            break;
        }
    }
    if out.is_empty() {
        out.push(homepage.to_string());
    }
    out
}

fn canonicalize_page_url(origin: &str, url: &str) -> Result<String, std::io::Error> {
    let trimmed = url.trim();
    let absolute = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else if trimmed.starts_with('/') {
        format!("{origin}{trimmed}")
    } else {
        format!("{origin}/{trimmed}")
    };
    Url::parse(&absolute)
        .map(|u| u.to_string())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))
}

pub fn parse_robots_sitemaps(robots_txt: &str) -> Vec<String> {
    robots_txt
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.to_ascii_lowercase().starts_with("sitemap:") {
                Some(line[8..].trim().to_string())
            } else {
                None
            }
        })
        .collect()
}

pub fn parse_robots_disallow(robots_txt: &str) -> Vec<String> {
    let mut disallows = Vec::new();
    let mut applies = false;
    for line in robots_txt.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("user-agent:") {
            let agent = line[11..].trim();
            applies = agent == "*" || agent.eq_ignore_ascii_case("skippr");
        } else if applies && lower.starts_with("disallow:") {
            let path = line[9..].trim();
            if !path.is_empty() {
                disallows.push(path.to_string());
            }
        }
    }
    disallows
}

pub fn path_disallowed(path: &str, disallows: &[String]) -> bool {
    disallows.iter().any(|prefix| path.starts_with(prefix))
}

fn fetch_sitemap_urls(
    fetcher: &dyn SitemapFetcher,
    sitemap_url: &str,
    origin: &str,
    respect_robots: bool,
) -> Result<Vec<CandidateUrl>, std::io::Error> {
    let body = match fetcher.fetch_text(sitemap_url) {
        Ok(b) => b,
        Err(_) => return Ok(Vec::new()),
    };
    let disallows = if respect_robots {
        fetcher
            .fetch_text(&format!("{origin}/robots.txt"))
            .map(|txt| parse_robots_disallow(&txt))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    parse_sitemap_xml(&body, origin, &disallows, 0)
}

fn looks_like_sitemap_xml(body: &str) -> bool {
    let trimmed = body.trim_start();
    trimmed.starts_with("<?xml")
        || trimmed.starts_with("<urlset")
        || trimmed.starts_with("<sitemapindex")
}

fn parse_sitemap_xml(
    xml: &str,
    _origin: &str,
    disallows: &[String],
    depth: usize,
) -> Result<Vec<CandidateUrl>, std::io::Error> {
    if !looks_like_sitemap_xml(xml) {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut in_loc = false;
    let mut loc_buf = String::new();
    let mut priority = 0.5f64;
    let mut lastmod: Option<String> = None;
    let mut tag_stack: VecDeque<String> = VecDeque::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(quick_xml::events::Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                tag_stack.push_back(name.clone());
                if name == "loc" {
                    in_loc = true;
                    loc_buf.clear();
                }
            }
            Ok(quick_xml::events::Event::Text(e)) => {
                if in_loc {
                    loc_buf.push_str(&e.unescape().unwrap_or_default());
                } else if tag_stack.back().is_some_and(|t| t == "priority") {
                    if let Ok(p) = e.unescape().unwrap_or_default().parse::<f64>() {
                        priority = p;
                    }
                } else if tag_stack.back().is_some_and(|t| t == "lastmod") {
                    lastmod = Some(e.unescape().unwrap_or_default().to_string());
                }
            }
            Ok(quick_xml::events::Event::End(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name == "loc" {
                    in_loc = false;
                    let loc = loc_buf.trim();
                    if loc.ends_with(".xml") || loc.contains("sitemap") && loc.ends_with(".xml") {
                        // sitemap index child — caller may recurse; keep flat for v1
                    } else if let Ok(parsed) = Url::parse(loc) {
                        let path = parsed.path();
                        if !path_disallowed(path, disallows) {
                            let page_depth = path.matches('/').count().saturating_sub(1);
                            out.push(CandidateUrl {
                                url: loc.to_string(),
                                priority,
                                lastmod: lastmod.clone(),
                                depth: depth + page_depth,
                            });
                        }
                    }
                    priority = 0.5;
                    lastmod = None;
                }
                tag_stack.pop_back();
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Err(e) => {
                tracing::warn!("skipping ill-formed sitemap XML (depth={depth}): {e}");
                break;
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

pub trait SitemapFetcher {
    fn fetch_text(&self, url: &str) -> Result<String, std::io::Error>;
}

pub struct HttpSitemapFetcher {
    client: reqwest::Client,
}

impl HttpSitemapFetcher {
    pub fn new() -> Result<Self, std::io::Error> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent("SkipprSiteQuality/1.0")
            .build()
            .map_err(std::io::Error::other)?;
        Ok(Self { client })
    }

    pub async fn fetch_text_async(&self, url: &str) -> Result<String, std::io::Error> {
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(std::io::Error::other)?;
        if !resp.status().is_success() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("HTTP {} for {url}", resp.status()),
            ));
        }
        resp.text().await.map_err(std::io::Error::other)
    }
}

pub async fn resolve_url_list_async(
    origin: &str,
    url_mode: UrlMode,
    url_list: &[String],
    max_pages: u32,
    fetcher: &HttpSitemapFetcher,
    respect_robots: bool,
) -> Result<Vec<String>, std::io::Error> {
    let cap = max_pages.max(1) as usize;
    match url_mode {
        UrlMode::UrlList => {
            let mut out: Vec<String> = url_list
                .iter()
                .map(|u| canonicalize_page_url(origin, u))
                .collect::<Result<Vec<_>, _>>()?;
            out.sort();
            out.dedup();
            out.truncate(cap);
            Ok(out)
        }
        UrlMode::TldSample => {
            let home = homepage_url(origin);
            let mut candidates = vec![CandidateUrl {
                url: home.clone(),
                priority: 1.0,
                lastmod: None,
                depth: 0,
            }];
            if let Ok(robots) = fetcher
                .fetch_text_async(&format!("{origin}/robots.txt"))
                .await
            {
                for sitemap in parse_robots_sitemaps(&robots) {
                    candidates.extend(
                        fetch_sitemap_urls_async(fetcher, &sitemap, origin, respect_robots).await?,
                    );
                }
            }
            for fallback in ["/sitemap.xml", "/sitemap_index.xml"] {
                let loc = format!("{origin}{fallback}");
                if candidates.len() >= cap * 4 {
                    break;
                }
                candidates
                    .extend(fetch_sitemap_urls_async(fetcher, &loc, origin, respect_robots).await?);
            }
            Ok(rank_and_cap_candidates(origin, &home, candidates, cap))
        }
        UrlMode::SiteCrawl => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "site_crawl url_mode requires resolve_site_crawl_urls",
        )),
    }
}

async fn fetch_sitemap_urls_async(
    fetcher: &HttpSitemapFetcher,
    sitemap_url: &str,
    origin: &str,
    respect_robots: bool,
) -> Result<Vec<CandidateUrl>, std::io::Error> {
    let body = match fetcher.fetch_text_async(sitemap_url).await {
        Ok(b) => b,
        Err(_) => return Ok(Vec::new()),
    };
    let disallows = if respect_robots {
        fetcher
            .fetch_text_async(&format!("{origin}/robots.txt"))
            .await
            .map(|txt| parse_robots_disallow(&txt))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    parse_sitemap_xml(&body, origin, &disallows, 0)
}

pub struct StaticSitemapFetcher {
    pub robots_txt: Option<String>,
    pub sitemaps: std::collections::HashMap<String, String>,
}

impl SitemapFetcher for StaticSitemapFetcher {
    fn fetch_text(&self, url: &str) -> Result<String, std::io::Error> {
        if url.ends_with("/robots.txt") {
            return self
                .robots_txt
                .clone()
                .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound));
        }
        self.sitemaps
            .get(url)
            .cloned()
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
    }
}

static SITEMAP_LOC_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<loc>\s*([^<]+)\s*</loc>").unwrap());

/// Lightweight XML loc extraction for tests without full parser setup.
pub fn extract_sitemap_locs(xml: &str) -> Vec<String> {
    SITEMAP_LOC_RE
        .captures_iter(xml)
        .filter_map(|c| c.get(1).map(|m| m.as_str().trim().to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampling_respects_cap_and_priority() {
        let origin = "https://example.com";
        let home = homepage_url(origin);
        let candidates = vec![
            CandidateUrl {
                url: format!("{origin}/blog/post-1"),
                priority: 0.3,
                lastmod: None,
                depth: 2,
            },
            CandidateUrl {
                url: format!("{origin}/pricing"),
                priority: 0.8,
                lastmod: None,
                depth: 1,
            },
            CandidateUrl {
                url: home.clone(),
                priority: 0.5,
                lastmod: None,
                depth: 0,
            },
        ];
        let out = rank_and_cap_candidates(origin, &home, candidates, 2);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], home);
        assert_eq!(out[1], format!("{origin}/pricing"));
    }

    #[test]
    fn url_list_mode_respects_cap() {
        let origin = "https://example.com";
        let fetcher = StaticSitemapFetcher {
            robots_txt: None,
            sitemaps: Default::default(),
        };
        let urls = (1..=10)
            .map(|i| format!("{origin}/page-{i}"))
            .collect::<Vec<_>>();
        let out = resolve_url_list(origin, UrlMode::UrlList, &urls, 3, &fetcher, true).unwrap();
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn robots_disallow_filters_paths() {
        let disallows = parse_robots_disallow("User-agent: *\nDisallow: /private\n");
        assert!(path_disallowed("/private/secret", &disallows));
        assert!(!path_disallowed("/public", &disallows));
    }
}
