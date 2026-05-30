use std::collections::{HashSet, VecDeque};

use url::Url;

use crate::origin::{normalize_url_for_crawl, SiteOrigin};
use crate::robots::{path_allowed, ParsedRobots};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UrlSource {
    Tld,
    Sitemap,
    InternalLink,
}

#[derive(Clone, Debug)]
pub struct QueuedUrl {
    pub url: String,
    pub depth: u32,
    pub source: UrlSource,
    pub parent_url: Option<String>,
}

pub struct CrawlQueue {
    queue: VecDeque<QueuedUrl>,
    seen: HashSet<String>,
    max_urls: usize,
    max_depth: u32,
}

impl CrawlQueue {
    pub fn new(max_urls: usize, max_depth: u32) -> Self {
        Self {
            queue: VecDeque::new(),
            seen: HashSet::new(),
            max_urls: max_urls.max(1),
            max_depth,
        }
    }

    pub fn seed(&mut self, url: impl Into<String>, source: UrlSource) {
        self.enqueue(url.into(), 0, source, None);
    }

    pub fn enqueue(
        &mut self,
        url: String,
        depth: u32,
        source: UrlSource,
        parent: Option<String>,
    ) {
        if depth > self.max_depth || self.seen.len() >= self.max_urls {
            return;
        }
        if !self.seen.insert(url.clone()) {
            return;
        }
        self.queue.push_back(QueuedUrl {
            url,
            depth,
            source,
            parent_url: parent,
        });
    }

    pub fn pop(&mut self) -> Option<QueuedUrl> {
        self.queue.pop_front()
    }

    pub fn discovered_count(&self) -> usize {
        self.seen.len()
    }

    pub fn enqueue_internal_links(
        &mut self,
        links: &[String],
        depth: u32,
        parent: &str,
        robots: Option<&ParsedRobots>,
        respect_robots: bool,
        user_agent: &str,
    ) {
        for link in links {
            if self.seen.len() >= self.max_urls {
                break;
            }
            if respect_robots {
                if let Some(parsed) = robots {
                    let path = Url::parse(link)
                        .ok()
                        .map(|u| u.path().to_string())
                        .unwrap_or_else(|| "/".into());
                    if !path_allowed(&path, parsed, user_agent) {
                        continue;
                    }
                }
            }
            self.enqueue(link.clone(), depth + 1, UrlSource::InternalLink, Some(parent.to_string()));
        }
    }
}

pub fn default_sitemap_probe_paths() -> Vec<String> {
    vec![
        "/sitemap.xml".into(),
        "/sitemap_index.xml".into(),
        "/sitemap-index.xml".into(),
    ]
}

pub fn probe_sitemap_urls(origin: &SiteOrigin, paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .map(|p| {
            let path = if p.starts_with('/') {
                p.clone()
            } else {
                format!("/{p}")
            };
            format!("{}{}", origin.origin, path)
        })
        .collect()
}

pub fn filter_site_urls(urls: impl IntoIterator<Item = String>, origin: &SiteOrigin) -> Vec<String> {
    urls.into_iter()
        .filter_map(|u| normalize_url_for_crawl(&u, origin))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::origin::normalize_site;

    #[test]
    fn queue_respects_max_urls() {
        let origin = normalize_site("https://example.com").unwrap();
        let mut q = CrawlQueue::new(2, 5);
        q.seed(format!("{}/", origin.origin), UrlSource::Tld);
        q.enqueue(
            format!("{}/a", origin.origin),
            1,
            UrlSource::InternalLink,
            None,
        );
        q.enqueue(
            format!("{}/b", origin.origin),
            1,
            UrlSource::InternalLink,
            None,
        );
        assert_eq!(q.discovered_count(), 2);
    }
}
