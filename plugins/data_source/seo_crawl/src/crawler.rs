use std::collections::{HashSet, VecDeque};

use crate::fetch::HttpFetcher;
use crate::html::parse_fetched_page;
use crate::origin::{normalize_site, SiteOrigin};
use crate::robots::{parse_robots_txt, path_allowed};

pub struct CrawlPageResult {
    pub url: String,
    pub parsed: crate::html::ParsedPage,
    pub status: u16,
}

async fn collect_sitemap_urls(
    origin: &SiteOrigin,
    fetcher: &HttpFetcher,
    parsed_robots: Option<&crate::robots::ParsedRobots>,
) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(rules) = parsed_robots {
        for sm in &rules.sitemap_urls {
            if let Some(url) = crate::origin::normalize_url_for_crawl(sm, origin) {
                out.push(url);
            }
        }
    }
    for path in ["/sitemap.xml", "/sitemap_index.xml"] {
        let sm_url = format!("{}{}", origin.origin, path);
        if let Ok(resp) = fetcher.get(&sm_url, origin).await {
            if let Ok(entries) = crate::sitemap::parse_sitemap_xml(&resp.body) {
                for entry in entries {
                    if let Some(url) = crate::origin::normalize_url_for_crawl(&entry.loc, origin) {
                        out.push(url);
                    }
                }
            }
        }
    }
    out
}

/// Crawl same-origin pages: BFS via discovered internal links first, then unfetched sitemap URLs.
pub async fn crawl_site(
    origin: &SiteOrigin,
    fetcher: &HttpFetcher,
    user_agent: &str,
    max_urls: u32,
    max_depth: u32,
    respect_robots: bool,
    seed_urls: &[String],
) -> Result<Vec<CrawlPageResult>, std::io::Error> {
    let cap = max_urls.max(1) as usize;
    let depth_cap = max_depth;
    let robots_url = format!("{}/robots.txt", origin.origin);
    let robots_body = fetcher.get(&robots_url, origin).await.ok().map(|r| r.body);
    let parsed_robots = robots_body.as_deref().map(parse_robots_txt);

    let mut queue: VecDeque<(String, u32)> = VecDeque::new();
    let mut seen = HashSet::new();
    let home = format!("{}/", origin.origin.trim_end_matches('/'));
    queue.push_back((home.clone(), 0));
    seen.insert(home);

    for seed in seed_urls {
        let trimmed = seed.trim();
        if trimmed.is_empty() {
            continue;
        }
        let url = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            trimmed.to_string()
        } else {
            let path = if trimmed.starts_with('/') {
                trimmed.to_string()
            } else {
                format!("/{trimmed}")
            };
            format!("{}{}", origin.origin.trim_end_matches('/'), path)
        };
        if let Some(url) = crate::origin::normalize_url_for_crawl(&url, origin) {
            if seen.insert(url.clone()) {
                queue.push_back((url, 0));
            }
        }
    }

    let mut sitemap_deferred: VecDeque<String> =
        collect_sitemap_urls(origin, fetcher, parsed_robots.as_ref())
            .await
            .into_iter()
            .filter(|u| !seen.contains(u))
            .collect();

    let mut results = Vec::new();
    while results.len() < cap {
        let (url, depth) = if let Some(next) = queue.pop_front() {
            next
        } else if let Some(sm) = sitemap_deferred.pop_front() {
            if !seen.insert(sm.clone()) {
                continue;
            }
            (sm, 0)
        } else {
            break;
        };

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
        let response = match fetcher.get(&url, origin).await {
            Ok(r) => r,
            Err(_) => continue,
        };
        if response.status >= 400 {
            continue;
        }
        let parsed = parse_fetched_page(&url, &response, origin);
        results.push(CrawlPageResult {
            url: url.clone(),
            parsed,
            status: response.status,
        });
        if depth < depth_cap {
            let page = results.last().unwrap();
            for link in &page.parsed.internal_links {
                if link.link_kind == "internal" && !link.is_nofollow {
                    if seen.insert(link.target_url.clone()) {
                        queue.push_back((link.target_url.clone(), depth + 1));
                    }
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
        let pages = crawl_site(&origin, &fetcher, "SkipprSeoCrawl/1.0", 5, 1, true, &[])
            .await
            .unwrap();
        assert!(!pages.is_empty());
        std::env::remove_var("SKIPPR_SEO_CRAWL_FIXTURE_DIR");
    }
}
