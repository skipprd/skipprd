use std::collections::HashMap;

use scraper::{Html, Selector};
use serde_json::{json, Value};
use url::Url;

use crate::fetch::{resolve_href, FetchResponse};
use crate::origin::SiteOrigin;

#[derive(Clone, Debug)]
pub struct ExtractedLink {
    pub target_url: String,
    pub anchor_text: String,
    pub rel: Vec<String>,
    pub is_nofollow: bool,
    pub is_ugc: bool,
    pub is_sponsored: bool,
}

#[derive(Clone, Debug)]
pub struct ParsedPage {
    pub canonical_url: String,
    pub h1: Option<String>,
    pub head: Value,
    pub links: Vec<ExtractedLink>,
    pub main_text: String,
    pub content_hash: String,
    pub structured_data_count: usize,
    pub issues: Vec<PageIssue>,
}

#[derive(Clone, Debug)]
pub struct PageIssue {
    pub code: String,
    pub severity: String,
    pub message: String,
    pub evidence: Value,
}

pub fn parse_html_page(
    page_url: &str,
    response: &FetchResponse,
    origin: &SiteOrigin,
) -> ParsedPage {
    let document = Html::parse_document(&response.body);
    let title = meta_content(&document, "title").or_else(|| {
        document
            .select(&Selector::parse("title").unwrap())
            .next()
            .map(|el| el.text().collect::<String>().trim().to_string())
    });
    let description = meta_name(&document, "description");
    let robots_meta = meta_name(&document, "robots");
    let canonical = link_rel(&document, "canonical");
    let html_lang = document
        .select(&Selector::parse("html").unwrap())
        .next()
        .and_then(|el| el.value().attr("lang").map(str::to_string));
    let html_dir = document
        .select(&Selector::parse("html").unwrap())
        .next()
        .and_then(|el| el.value().attr("dir").map(str::to_string));

    let mut og = serde_json::Map::new();
    let mut twitter = serde_json::Map::new();
    for meta in document.select(&Selector::parse("meta[property^=og:], meta[name^=twitter:]").unwrap())
    {
        if let Some(prop) = meta.value().attr("property").or(meta.value().attr("name")) {
            if let Some(content) = meta.value().attr("content") {
                if prop.starts_with("og:") {
                    og.insert(prop.to_string(), json!(content));
                } else if prop.starts_with("twitter:") {
                    twitter.insert(prop.to_string(), json!(content));
                }
            }
        }
    }

    let h1 = document
        .select(&Selector::parse("h1").unwrap())
        .next()
        .map(|el| el.text().collect::<String>().trim().to_string());

    let hreflang: Vec<Value> = document
        .select(&Selector::parse("link[rel=alternate][hreflang]").unwrap())
        .filter_map(|el| {
            let href = el.value().attr("href")?;
            let lang = el.value().attr("hreflang")?;
            Some(json!({ "hreflang": lang, "href": href }))
        })
        .collect();

    let head = json!({
        "title": title,
        "title_length": title.as_ref().map(|t| t.chars().count()),
        "meta_description": description,
        "meta_description_length": description.as_ref().map(|d| d.chars().count()),
        "meta_robots": robots_meta,
        "canonical": canonical,
        "html_lang": html_lang,
        "html_dir": html_dir,
        "viewport_present": meta_name(&document, "viewport").is_some(),
        "open_graph": og,
        "twitter": twitter,
        "hreflang": hreflang,
        "http": response_headers_summary(response),
    });

    let main_text = extract_main_text(&document);
    let content_hash = content_sha256(&main_text);

    let links = extract_links(&document, page_url, origin);
    let structured_data_count = document
        .select(&Selector::parse("script[type='application/ld+json']").unwrap())
        .count();

    let mut issues = Vec::new();
    if title.as_ref().is_none_or(|t| t.is_empty()) {
        issues.push(PageIssue {
            code: "missing_title".into(),
            severity: "high".into(),
            message: "Page is missing a <title>".into(),
            evidence: json!({ "url": page_url }),
        });
    }
    if description.as_ref().is_none_or(|d| d.is_empty()) {
        issues.push(PageIssue {
            code: "missing_meta_description".into(),
            severity: "medium".into(),
            message: "Page is missing meta description".into(),
            evidence: json!({ "url": page_url }),
        });
    }
    if let (Some(canon), final_url) = (&canonical, &response.final_url) {
        if canon != final_url {
            issues.push(PageIssue {
                code: "canonical_mismatch_final_url".into(),
                severity: "medium".into(),
                message: "Canonical href does not match final URL after redirects".into(),
                evidence: json!({ "canonical": canon, "final_url": final_url }),
            });
        }
    }

    let canonical_url = canonical
        .clone()
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| response.final_url.clone());

    ParsedPage {
        canonical_url,
        h1,
        head,
        links,
        main_text,
        content_hash,
        structured_data_count,
        issues,
    }
}

fn response_headers_summary(response: &FetchResponse) -> Value {
    json!({
        "content_type": response.headers.get("content-type"),
        "cache_control": response.headers.get("cache-control"),
        "etag": response.headers.get("etag"),
        "last_modified": response.headers.get("last-modified"),
        "x_robots_tag": response.headers.get("x-robots-tag"),
        "strict_transport_security": response.headers.get("strict-transport-security"),
        "redirect_chain": response.redirect_chain,
        "ttfb_ms": response.ttfb_ms,
        "status": response.status,
    })
}

fn meta_name(document: &Html, name: &str) -> Option<String> {
    let selector = Selector::parse(&format!("meta[name='{name}'], meta[name=\"{name}\"]")).ok()?;
    document
        .select(&selector)
        .next()
        .and_then(|el| el.value().attr("content").map(str::to_string))
}

fn meta_content(document: &Html, _name: &str) -> Option<String> {
    None
}

fn link_rel(document: &Html, rel: &str) -> Option<String> {
    document
        .select(&Selector::parse(&format!("link[rel='{rel}'], link[rel=\"{rel}\"]")).unwrap())
        .next()
        .and_then(|el| el.value().attr("href").map(str::to_string))
}

fn extract_main_text(document: &Html) -> String {
    let body_sel = Selector::parse("body").unwrap();
    let mut parts = Vec::new();
    if let Some(body) = document.select(&body_sel).next() {
        for sel in ["main", "article", "[role=main]"] {
            if let Ok(s) = Selector::parse(sel) {
                for el in body.select(&s) {
                    let text = el.text().collect::<Vec<_>>().join(" ");
                    let normalized = normalize_whitespace(&text);
                    if !normalized.is_empty() {
                        parts.push(normalized);
                    }
                }
            }
        }
    }
    if parts.is_empty() {
        let text = document.root_element().text().collect::<Vec<_>>().join(" ");
        normalize_whitespace(&text)
    } else {
        parts.join("\n\n")
    }
}

pub fn normalize_whitespace(text: &str) -> String {
    let mut out = String::new();
    let mut prev_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out.trim().to_string()
}

pub fn content_sha256(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(text.as_bytes());
    format!("sha256:{:x}", digest)
}

fn extract_links(document: &Html, page_url: &str, origin: &SiteOrigin) -> Vec<ExtractedLink> {
    let mut links = Vec::new();
    let a_sel = Selector::parse("a[href]").unwrap();
    for el in document.select(&a_sel) {
        let Some(href) = el.value().attr("href") else {
            continue;
        };
        if href.starts_with('#') || href.starts_with("mailto:") || href.starts_with("tel:") {
            continue;
        }
        let Some(target) = resolve_href(href, page_url, origin) else {
            continue;
        };
        let rel_attr = el.value().attr("rel").unwrap_or_default();
        let rel: Vec<String> = rel_attr
            .split_whitespace()
            .map(|s| s.to_ascii_lowercase())
            .collect();
        let anchor_text = el.text().collect::<String>().trim().to_string();
        links.push(ExtractedLink {
            target_url: target,
            anchor_text,
            rel: rel.clone(),
            is_nofollow: rel.iter().any(|r| r == "nofollow"),
            is_ugc: rel.iter().any(|r| r == "ugc"),
            is_sponsored: rel.iter().any(|r| r == "sponsored"),
        });
    }
    links
}

pub fn technical_score(response: &FetchResponse, page: &ParsedPage) -> f64 {
    let mut score = 1.0f64;
    if response.status >= 400 {
        score -= 0.5;
    }
    if page.h1.is_none() {
        score -= 0.1;
    }
    if page.head.get("title").and_then(|v| v.as_str()).is_none() {
        score -= 0.15;
    }
    score.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::HttpFetcher;
    use crate::origin::normalize_site;

    #[tokio::test]
    async fn parses_fixture_head_fields() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        let origin = normalize_site("https://example.com").unwrap();
        let fetcher = HttpFetcher::new("test").with_fixture_dir(dir);
        let resp = fetcher
            .get("https://example.com/", &origin)
            .await
            .unwrap();
        let page = parse_html_page("https://example.com/", &resp, &origin);
        assert!(page.head.get("title").is_some());
        assert!(!page.content_hash.is_empty());
    }
}
