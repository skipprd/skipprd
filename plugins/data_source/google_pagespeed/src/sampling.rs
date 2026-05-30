use std::collections::{HashSet, VecDeque};

use serde::Deserialize;
use serde_derive::Serialize;
use tracing::warn;
use url::Url;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UrlMode {
    TldSample,
    UrlList,
}

#[derive(Debug, Clone)]
pub struct SamplingInput<'a> {
    pub site: &'a str,
    pub url_mode: UrlMode,
    pub url_list: &'a [String],
    pub max_urls: u32,
    pub respect_robots: bool,
    pub fixture_dir: Option<&'a str>,
}

pub fn sample_urls(input: SamplingInput<'_>) -> Result<Vec<String>, std::io::Error> {
    match input.url_mode {
        UrlMode::UrlList => sample_url_list(input.site, input.url_list, input.max_urls),
        UrlMode::TldSample => sample_tld(input),
    }
}

fn sample_url_list(
    site: &str,
    url_list: &[String],
    max_urls: u32,
) -> Result<Vec<String>, std::io::Error> {
    let base = normalize_site(site)?;
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for raw in url_list {
        let canonical = canonicalize_url(raw, &base)?;
        if seen.insert(canonical.clone()) {
            out.push(canonical);
        }
        if out.len() >= max_urls as usize {
            break;
        }
    }
    if out.is_empty() {
        out.push(base);
    }
    Ok(out)
}

fn sample_tld(input: SamplingInput<'_>) -> Result<Vec<String>, std::io::Error> {
    if let Some(dir) = input.fixture_dir.filter(|d| !d.trim().is_empty()) {
        return sample_tld_from_fixtures(dir, input.site, input.max_urls);
    }

    let base = normalize_site(input.site)?;
    let mut candidates: Vec<(i32, String)> = Vec::new();
    candidates.push((0, base.clone()));

    if input.respect_robots {
        match fetch_robots_sitemaps(&base) {
            Ok(sitemaps) => {
                for sm in sitemaps {
                    if let Ok(urls) = fetch_sitemap_urls(&sm) {
                        for url in urls {
                            if let Ok(c) = canonicalize_url(&url, &base) {
                                let priority = url_priority(&c, &base);
                                candidates.push((priority, c));
                            }
                        }
                    }
                }
            }
            Err(err) => {
                warn!("PageSpeed robots/sitemap discovery failed for {base}: {err}");
            }
        }
    }

    candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (_, url) in candidates {
        if seen.insert(url.clone()) {
            out.push(url);
        }
        if out.len() >= input.max_urls as usize {
            break;
        }
    }
    if out.is_empty() {
        out.push(base);
    }
    Ok(out)
}

fn sample_tld_from_fixtures(
    dir: &str,
    site: &str,
    max_urls: u32,
) -> Result<Vec<String>, std::io::Error> {
    let base = normalize_site(site)?;
    let path = format!("{}/sample_urls.json", dir.trim_end_matches('/'));
    if let Ok(bytes) = std::fs::read(&path) {
        let urls: Vec<String> =
            serde_json::from_slice(&bytes).map_err(|e| std::io::Error::other(e.to_string()))?;
        return sample_url_list(site, &urls, max_urls);
    }
    Ok(vec![base])
}

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
            format!("invalid site URL '{site}': {e}"),
        )
    })?;
    let mut normalized = format!(
        "{}://{}",
        parsed.scheme(),
        parsed.host_str().unwrap_or_default()
    );
    if let Some(port) = parsed.port() {
        normalized.push(':');
        normalized.push_str(&port.to_string());
    }
    let path = parsed.path();
    if path != "/" && !path.is_empty() {
        normalized.push_str(path.trim_end_matches('/'));
    } else {
        normalized.push('/');
    }
    Ok(normalized)
}

pub fn canonicalize_url(raw: &str, base: &str) -> Result<String, std::io::Error> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "empty URL",
        ));
    }
    let absolute = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else if trimmed.starts_with('/') {
        let base_url = Url::parse(base).map_err(|e| std::io::Error::other(e.to_string()))?;
        base_url
            .join(trimmed)
            .map(|u| u.to_string())
            .map_err(|e| std::io::Error::other(e.to_string()))?
    } else {
        format!("{}/{}", base.trim_end_matches('/'), trimmed.trim_start_matches('/'))
    };
    let parsed = Url::parse(&absolute).map_err(|e| std::io::Error::other(e.to_string()))?;
    let mut out = format!(
        "{}://{}{}",
        parsed.scheme(),
        parsed.host_str().unwrap_or_default(),
        parsed.path()
    );
    if let Some(query) = parsed.query() {
        if !query.is_empty() {
            out.push('?');
            out.push_str(query);
        }
    }
    Ok(out)
}

fn url_priority(url: &str, base: &str) -> i32 {
    if url == base || url.trim_end_matches('/') == base.trim_end_matches('/') {
        return 0;
    }
    let path = Url::parse(url)
        .ok()
        .map(|u| u.path().to_string())
        .unwrap_or_default();
    let depth = path.split('/').filter(|s| !s.is_empty()).count();
    10 + depth as i32
}

fn fetch_robots_sitemaps(site: &str) -> Result<Vec<String>, std::io::Error> {
    let robots_url = format!("{}robots.txt", site.trim_end_matches('/'));
    let body = blocking_get(&robots_url)?;
    let mut sitemaps = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if line.to_ascii_lowercase().starts_with("sitemap:") {
            if let Some(url) = line.splitn(2, ':').nth(1) {
                let sm = url.trim();
                if !sm.is_empty() {
                    sitemaps.push(sm.to_string());
                }
            }
        }
    }
    Ok(sitemaps)
}

fn fetch_sitemap_urls(sitemap_url: &str) -> Result<Vec<String>, std::io::Error> {
    let body = blocking_get(sitemap_url)?;
    let mut out = Vec::new();
    if body.contains("<urlset") || body.contains("<sitemapindex") {
        parse_sitemap_xml(&body, &mut out)?;
    } else {
        for line in body.lines() {
            let line = line.trim();
            if line.starts_with("http://") || line.starts_with("https://") {
                out.push(line.to_string());
            }
        }
    }
    Ok(out)
}

fn parse_sitemap_xml(body: &str, out: &mut Vec<String>) -> Result<(), std::io::Error> {
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
            Err(e) => {
                return Err(std::io::Error::other(format!("sitemap XML parse error: {e}")));
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(())
}

fn blocking_get(url: &str) -> Result<String, std::io::Error> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let response = client
        .get(url)
        .send()
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    if !response.status().is_success() {
        return Err(std::io::Error::other(format!(
            "HTTP {} fetching {url}",
            response.status()
        )));
    }
    response
        .text()
        .map_err(|e| std::io::Error::other(e.to_string()))
}

pub fn apply_request_budget(
    urls: Vec<String>,
    strategies: &[crate::config::Strategy],
    max_requests: u32,
) -> Vec<(String, crate::config::Strategy)> {
    let mut jobs = Vec::new();
    for url in urls {
        for strategy in strategies {
            jobs.push((url.clone(), strategy.clone()));
        }
    }
    if jobs.len() <= max_requests as usize {
        return jobs;
    }
    let per_url = strategies.len().max(1);
    let max_urls = (max_requests as usize / per_url).max(1);
    let mut truncated_urls = Vec::new();
    let mut seen = HashSet::new();
    for (url, _) in &jobs {
        if seen.insert(url.clone()) {
            truncated_urls.push(url.clone());
        }
        if truncated_urls.len() >= max_urls {
            break;
        }
    }
    let mut out = Vec::new();
    for url in truncated_urls {
        for strategy in strategies {
            out.push((url.clone(), strategy.clone()));
            if out.len() >= max_requests as usize {
                return out;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Strategy;

    #[test]
    fn sampling_cap_never_exceeds_max_urls() {
        let urls: Vec<String> = (0..100)
            .map(|i| format!("https://example.com/page{i}"))
            .collect();
        let sampled = sample_url_list("https://example.com/", &urls, 10).unwrap();
        assert_eq!(sampled.len(), 10);
    }

    #[test]
    fn request_budget_truncates_lowest_priority_urls() {
        let urls: Vec<String> = (0..20)
            .map(|i| format!("https://example.com/p{i}"))
            .collect();
        let strategies = vec![Strategy::Mobile, Strategy::Desktop];
        let jobs = apply_request_budget(urls, &strategies, 6);
        assert_eq!(jobs.len(), 6);
        assert_eq!(jobs.iter().filter(|(_, s)| *s == Strategy::Mobile).count(), 3);
    }

    #[test]
    fn normalize_site_adds_scheme_and_trailing_slash() {
        let url = normalize_site("example.com").unwrap();
        assert_eq!(url, "https://example.com/");
    }

    #[test]
    fn fixture_sample_urls_json() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures");
        let urls = sample_tld(SamplingInput {
            site: "https://example.com",
            url_mode: UrlMode::TldSample,
            url_list: &[],
            max_urls: 5,
            respect_robots: true,
            fixture_dir: Some(dir),
        })
        .unwrap();
        assert!(urls.len() >= 2);
        assert!(urls[0].contains("example.com"));
    }
}
