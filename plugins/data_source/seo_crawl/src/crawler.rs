use std::collections::{HashSet, VecDeque};

use crate::fetch::HttpFetcher;
use crate::html::parse_html_page;
use crate::origin::{normalize_site, SiteOrigin};
use crate::robots::{parse_robots_txt, path_allowed};

pub struct CrawlPageResult {
    pub url: String,
    pub parsed: crate::html::ParsedPage,
    pub status: u16,
}

pub async fn crawl_site(
    origin: &SiteOrigin,
    fetcher: &HttpFetcher,
    user_agent: &str,
    max_urls: u32,
    max_depth: u32,
    respect_robots: bool,
) -> Result<Vec<CrawlPageResult>, std::io::Error> {
    let cap = max_urls.max(1) as usize;
    let depth_cap = max_depth;
    let robots_url = format!("{}/robots.txt", origin.origin);
    let robots_body = fetcher.get(&robots_url, origin).await.ok().map(|r| r.body);
    let parsed_robots = robots_body
        .as_deref()
        .map(parse_robots_txt);

    let mut queue: VecDeque<(String, u32)> = VecDeque::new();
    let mut seen = HashSet::new();
    let home = format!("{}/", origin.origin.trim_end_matches('/'));
    queue.push_back((home.clone(), 0));
    seen.insert(home);

    if let Some(rules) = parsed_robots.as_ref() {
        for sm in &rules.sitemap_urls {
            if let Some(url) = crate::origin::normalize_url_for_crawl(sm, origin) {
                if seen.insert(url.clone()) {
                    queue.push_back((url, 0));
                }
            }
        }
    }
    for path in ["/sitemap.xml", "/sitemap_index.xml"] {
        let sm_url = format!("{}{}", origin.origin, path);
        if let Ok(resp) = fetcher.get(&sm_url, origin).await {
            if let Ok(entries) = crate::sitemap::parse_sitemap_xml(&resp.body) {
                for entry in entries {
                    if let Some(url) = crate::origin::normalize_url_for_crawl(&entry.loc, origin) {
                        if seen.insert(url.clone()) {
                            queue.push_back((url, 0));
                        }
                    }
                }
            }
        }
    }

    let mut results = Vec::new();
    while let Some((url, depth)) = queue.pop_front() {
        if results.len() >= cap {
            break;
        }
        if depth > depth_cap {
            continue;
        }
        let path = url::Url::parse(&url)
            .ok()
            .map(|u| u.path().to_string())
            .unwrap_or_else(|| "/".into());
        if respect_robots {
            if let Some(rules) = parsed_robots.as_ref() {
                if !path_allowed(&path, rules, user_agent) {
                    continue;
                }
            }
        }
        let response = fetcher.get(&url, origin).await?;
        if response.status >= 400 {
            continue;
        }
        let parsed = parse_html_page(&url, &response, origin);
        results.push(CrawlPageResult {
            url: url.clone(),
            parsed,
            status: response.status,
        });
        if depth < depth_cap {
            let page = results.last().unwrap();
            for link in &page.parsed.links {
                if seen.insert(link.target_url.clone()) {
                    queue.push_back((link.target_url.clone(), depth + 1));
                }
            }
        }
    }
    Ok(results)
}

pub fn seed_origin(site: &str) -> Result<SiteOrigin, std::io::Error> {
    normalize_site(site)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fixture_crawl_returns_at_least_homepage() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        std::env::set_var("SKIPPR_SEO_CRAWL_FIXTURE_DIR", dir);
        let origin = seed_origin("https://example.com").unwrap();
        let fetcher = HttpFetcher::new("SkipprSeoCrawl/1.0").with_fixture_dir(dir);
        let pages = crawl_site(&origin, &fetcher, "SkipprSeoCrawl/1.0", 5, 1, true)
            .await
            .unwrap();
        assert!(!pages.is_empty());
        std::env::remove_var("SKIPPR_SEO_CRAWL_FIXTURE_DIR");
    }
}
