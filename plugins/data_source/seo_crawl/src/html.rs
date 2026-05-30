use std::collections::HashMap;

use regex::Regex;
use scraper::{Html, Selector};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::fetch::{resolve_href, FetchResponse};
use crate::origin::SiteOrigin;

#[derive(Debug, Clone)]
pub struct ParsedLink {
    pub target_url: String,
    pub anchor_text: String,
    pub rel: Vec<String>,
    pub is_nofollow: bool,
}

#[derive(Debug, Clone)]
pub struct IssueRow {
    pub issue_code: String,
    pub message: String,
    pub severity: String,
}

#[derive(Debug, Clone)]
pub struct ParsedPage {
    pub canonical_url: String,
    pub title: Option<String>,
    pub meta_description: Option<String>,
    pub h1: Option<String>,
    pub head: Value,
    pub links: Vec<ParsedLink>,
    pub main_text: String,
    pub content_hash: String,
    pub structured_data_count: u32,
    pub issues: Vec<IssueRow>,
}

pub fn normalize_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn content_hash(text: &str) -> String {
    let normalized = normalize_whitespace(text);
    let digest = Sha256::digest(normalized.as_bytes());
    format!("sha256:{:x}", digest)
}

pub fn parse_html_page(url: &str, response: &FetchResponse, origin: &SiteOrigin) -> ParsedPage {
    let document = Html::parse_document(&response.body);
    let title = select_text(&document, "title");
    let meta_description = meta_content(&document, "description");
    let h1 = select_text(&document, "h1");
    let head = extract_head_json(&document, &response.final_url, &title, &meta_description, &h1);
    let main_text = extract_main_text(&document);
    let hash = content_hash(&main_text);
    let links = extract_links(&document, url, origin);
    let structured_data_count = document
        .select(&Selector::parse("script[type=\"application/ld+json\"]").unwrap())
        .count() as u32;
    let mut issues = Vec::new();
    if title.as_ref().map(|t| t.is_empty()).unwrap_or(true) {
        issues.push(IssueRow {
            issue_code: "MISSING_TITLE".into(),
            message: "Page is missing a title element".into(),
            severity: "high".into(),
        });
    }
    if meta_description.as_ref().map(|d| d.is_empty()).unwrap_or(true) {
        issues.push(IssueRow {
            issue_code: "MISSING_META_DESCRIPTION".into(),
            message: "Page is missing meta description".into(),
            severity: "medium".into(),
        });
    }
    if h1.as_ref().map(|h| h.is_empty()).unwrap_or(true) {
        issues.push(IssueRow {
            issue_code: "MISSING_H1".into(),
            message: "Page is missing H1".into(),
            severity: "medium".into(),
        });
    }
    let canonical_url = head
        .get("canonical")
        .and_then(|v| v.as_str())
        .unwrap_or(&response.final_url)
        .to_string();
    ParsedPage {
        canonical_url,
        title,
        meta_description,
        h1,
        head,
        links,
        main_text,
        content_hash: hash,
        structured_data_count,
        issues,
    }
}

pub fn technical_score(response: &FetchResponse, page: &ParsedPage) -> f64 {
    let mut score = 1.0;
    if response.status >= 400 {
        score -= 0.5;
    }
    for issue in &page.issues {
        score -= match issue.severity.as_str() {
            "high" => 0.25,
            "medium" => 0.1,
            _ => 0.05,
        };
    }
    if page.structured_data_count == 0 {
        score -= 0.05;
    }
    score.clamp(0.0, 1.0)
}

fn select_text(document: &Html, selector: &str) -> Option<String> {
    let sel = Selector::parse(selector).ok()?;
    document
        .select(&sel)
        .next()
        .map(|el| el.text().collect::<String>().trim().to_string())
        .filter(|s| !s.is_empty())
}

fn meta_content(document: &Html, name: &str) -> Option<String> {
    let sel = Selector::parse(&format!("meta[name=\"{name}\"]")).ok()?;
    document
        .select(&sel)
        .next()
        .and_then(|el| el.value().attr("content"))
        .map(|c| c.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn extract_head_json(
    document: &Html,
    final_url: &str,
    title: &Option<String>,
    meta_description: &Option<String>,
    h1: &Option<String>,
) -> Value {
    let mut head = serde_json::Map::new();
    if let Some(t) = title {
        head.insert("title".into(), json!(t));
    }
    if let Some(d) = meta_description {
        head.insert("meta_description".into(), json!(d));
    }
    if let Some(h) = h1 {
        head.insert("h1".into(), json!(h));
    }
    if let Some(canonical) = select_attr(document, "link[rel=\"canonical\"]", "href") {
        head.insert("canonical".into(), json!(canonical));
        if canonical != final_url {
            head.insert("canonical_mismatch".into(), json!(true));
        }
    }
    for prop in ["og:title", "og:description", "og:url", "og:image", "twitter:card"] {
        if let Some(val) = meta_property(document, prop) {
            head.insert(prop.replace(':', "_"), json!(val));
        }
    }
    if let Some(lang) = select_attr(document, "html", "lang") {
        head.insert("html_lang".into(), json!(lang));
    }
    Value::Object(head)
}

fn select_attr(document: &Html, selector: &str, attr: &str) -> Option<String> {
    let sel = Selector::parse(selector).ok()?;
    document
        .select(&sel)
        .next()
        .and_then(|el| el.value().attr(attr))
        .map(str::to_string)
}

fn meta_property(document: &Html, property: &str) -> Option<String> {
    let sel = Selector::parse(&format!("meta[property=\"{property}\"]")).ok()?;
    document
        .select(&sel)
        .next()
        .and_then(|el| el.value().attr("content"))
        .map(str::to_string)
}

fn extract_main_text(document: &Html) -> String {
    let body_sel = Selector::parse("body").ok();
    let Some(body_sel) = body_sel else {
        return String::new();
    };
    document
        .select(&body_sel)
        .next()
        .map(|el| el.text().collect::<String>())
        .unwrap_or_default()
}

fn extract_links(document: &Html, page_url: &str, origin: &SiteOrigin) -> Vec<ParsedLink> {
    let sel = Selector::parse("a[href]").ok();
    let Some(sel) = sel else {
        return Vec::new();
    };
    let mut links = Vec::new();
    for anchor in document.select(&sel) {
        let href = anchor.value().attr("href").unwrap_or_default();
        if href.is_empty() || href.starts_with('#') || href.starts_with("mailto:") {
            continue;
        }
        let Some(target_url) = resolve_href(href, page_url, origin) else {
            continue;
        };
        let rel: Vec<String> = anchor
            .value()
            .attr("rel")
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_string)
            .collect();
        let is_nofollow = rel.iter().any(|r| r.eq_ignore_ascii_case("nofollow"));
        links.push(ParsedLink {
            target_url,
            anchor_text: anchor.text().collect::<String>().trim().to_string(),
            rel,
            is_nofollow,
        });
    }
    links
}

fn question_re() -> Regex {
    Regex::new(r"^(how|what|why|when|where|who)\b").expect("regex")
}

pub fn heading_blocks(page_url: &str, document: &Html) -> Vec<(String, String)> {
    let heading_sel = Selector::parse("h2, h3, h4, h5, h6").ok();
    let Some(heading_sel) = heading_sel else {
        return Vec::new();
    };
    let mut blocks = Vec::new();
    for heading in document.select(&heading_sel) {
        let text = heading.text().collect::<String>().trim().to_string();
        if text.is_empty() {
            continue;
        }
        let block_type = if text.contains('?') || question_re().is_match(&text.to_lowercase()) {
            "direct_answer"
        } else {
            "heading_section"
        };
        blocks.push((block_type.to_string(), text));
    }
    let _ = page_url;
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn content_hash_is_stable() {
        assert_eq!(content_hash("hello world"), content_hash("hello   world"));
    }

    #[test]
    fn parse_html_extracts_title_and_links() {
        let html = r#"<!doctype html><html lang="en"><head>
            <title>Example</title>
            <meta name="description" content="Desc">
            <link rel="canonical" href="https://example.com/">
        </head><body><h1>Home</h1><h2>What is SEO?</h2><p>Answer here.</p>
        <a href="/about">About</a></body></html>"#;
        let origin = crate::origin::normalize_site("https://example.com").unwrap();
        let response = FetchResponse {
            final_url: "https://example.com/".into(),
            status: 200,
            headers: HashMap::new(),
            body: html.into(),
            redirect_chain: vec![200],
            ttfb_ms: 10,
        };
        let parsed = parse_html_page("https://example.com/", &response, &origin);
        assert_eq!(parsed.title.as_deref(), Some("Example"));
        assert!(!parsed.links.is_empty());
    }
}
