use std::collections::HashSet;

use regex::Regex;
use serde_json::Value;
use url::Url;

use crate::origin::host_label;

static URL_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r#"https?://[^\s\)\]\"'<>]+"#).expect("url regex"));

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionSpan {
    pub mention_id: String,
    pub brand_name: String,
    pub matched_text: String,
    pub start_offset: usize,
    pub end_offset: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlRef {
    pub ref_id: String,
    pub url: String,
    pub title: Option<String>,
    pub snippet: Option<String>,
    pub anchor_text: Option<String>,
    pub source: String,
}

pub fn extract_urls_from_text(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut urls = Vec::new();
    for cap in URL_RE.find_iter(text) {
        let url = cap
            .as_str()
            .trim_end_matches(&['.', ',', ';', ')', ']'][..])
            .to_string();
        if url.is_empty() {
            continue;
        }
        if seen.insert(url.clone()) {
            urls.push(url);
        }
    }
    urls
}

pub fn extract_mentions(text: &str, brand_names: &[String]) -> Vec<MentionSpan> {
    let lower = text.to_lowercase();
    let mut mentions = Vec::new();
    let mut seen = HashSet::new();
    for brand in brand_names {
        let needle = brand.trim();
        if needle.is_empty() {
            continue;
        }
        let brand_lower = needle.to_lowercase();
        let mut start = 0usize;
        while let Some(pos) = lower[start..].find(&brand_lower) {
            let abs_start = start + pos;
            let abs_end = abs_start + brand_lower.len();
            let matched = text.get(abs_start..abs_end).unwrap_or(needle).to_string();
            let mention_id = format!("mention:{brand_lower}:{abs_start}");
            if seen.insert(mention_id.clone()) {
                mentions.push(MentionSpan {
                    mention_id,
                    brand_name: needle.to_string(),
                    matched_text: matched,
                    start_offset: abs_start,
                    end_offset: abs_end,
                });
            }
            start = abs_end;
        }
    }
    mentions
}

pub fn citations_from_json(raw: &Value) -> Vec<UrlRef> {
    let mut out = Vec::new();
    if let Some(arr) = raw.get("citations").and_then(|v| v.as_array()) {
        for (idx, item) in arr.iter().enumerate() {
            let url = item
                .get("url")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let Some(url) = url else { continue };
            out.push(UrlRef {
                ref_id: format!("citation:json:{idx}"),
                url: url.to_string(),
                title: item
                    .get("title")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                snippet: item
                    .get("snippet")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                anchor_text: None,
                source: "model_json".into(),
            });
        }
    }
    out
}

pub fn links_from_json(raw: &Value) -> Vec<UrlRef> {
    let mut out = Vec::new();
    if let Some(arr) = raw.get("links").and_then(|v| v.as_array()) {
        for (idx, item) in arr.iter().enumerate() {
            let url = item
                .get("url")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let Some(url) = url else { continue };
            out.push(UrlRef {
                ref_id: format!("link:json:{idx}"),
                url: url.to_string(),
                title: None,
                snippet: None,
                anchor_text: item
                    .get("anchor_text")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                source: "model_json".into(),
            });
        }
    }
    out
}

pub fn links_from_text(text: &str) -> Vec<UrlRef> {
    extract_urls_from_text(text)
        .into_iter()
        .enumerate()
        .map(|(idx, url)| UrlRef {
            ref_id: format!("link:text:{idx}"),
            url,
            title: None,
            snippet: None,
            anchor_text: None,
            source: "answer_text".into(),
        })
        .collect()
}

pub fn merge_url_refs(mut primary: Vec<UrlRef>, secondary: Vec<UrlRef>) -> Vec<UrlRef> {
    let mut seen: HashSet<String> = primary.iter().map(|r| r.url.clone()).collect();
    for item in secondary {
        if seen.insert(item.url.clone()) {
            primary.push(item);
        }
    }
    primary
}

pub fn url_matches_target_site(url: &str, site_origin: &str) -> bool {
    let target_host = host_label(site_origin).to_lowercase();
    Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_lowercase()))
        .is_some_and(|host| host == target_host || host.ends_with(&format!(".{target_host}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_urls_from_answer() {
        let urls = extract_urls_from_text("See https://example.com/docs and http://foo.bar/baz.");
        assert_eq!(urls.len(), 2);
        assert!(urls[0].contains("example.com"));
    }

    #[test]
    fn finds_brand_mentions() {
        let mentions = extract_mentions("Picnic is great. picnic app works.", &["Picnic".into()]);
        assert_eq!(mentions.len(), 2);
    }

    #[test]
    fn target_domain_match() {
        assert!(url_matches_target_site(
            "https://www.example.com/page",
            "https://example.com"
        ));
        assert!(!url_matches_target_site(
            "https://other.com",
            "https://example.com"
        ));
    }

    #[test]
    fn citations_from_json_skips_empty_urls() {
        let raw = serde_json::json!({
            "citations": [
                {"url": "https://example.com/a"},
                {"url": ""},
                {"title": "no url"}
            ]
        });
        let cites = citations_from_json(&raw);
        assert_eq!(cites.len(), 1);
        assert_eq!(cites[0].url, "https://example.com/a");
    }

    #[test]
    fn merge_url_refs_deduplicates_by_url() {
        let a = UrlRef {
            ref_id: "a".into(),
            url: "https://example.com".into(),
            title: None,
            snippet: None,
            anchor_text: None,
            source: "model_json".into(),
        };
        let b = UrlRef {
            ref_id: "b".into(),
            url: "https://example.com".into(),
            title: None,
            snippet: None,
            anchor_text: None,
            source: "answer_text".into(),
        };
        let merged = merge_url_refs(vec![a], vec![b]);
        assert_eq!(merged.len(), 1);
    }
}
