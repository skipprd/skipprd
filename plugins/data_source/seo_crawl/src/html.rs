use regex::Regex;
use scraper::{Html, Selector};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::fetch::{resolve_href, FetchResponse};
use crate::origin::SiteOrigin;

#[derive(Debug, Clone)]
pub struct HeadingEntry {
    pub level: u8,
    pub text: String,
}

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
    pub h1_count: u32,
    pub heading_outline: Vec<HeadingEntry>,
    pub img_count: u32,
    pub img_missing_alt: u32,
    pub head: Value,
    pub http_headers: Value,
    pub links: Vec<ParsedLink>,
    pub internal_links: Vec<LinkEdge>,
    pub main_text: String,
    pub content_hash: String,
    pub structured_data_count: u32,
    pub has_faq_schema: bool,
    pub technical_score: f64,
    pub issues: Vec<IssueRow>,
}

#[derive(Debug, Clone)]
pub struct LinkEdge {
    pub target_url: String,
    pub anchor_text: String,
    pub rel: Vec<String>,
    pub is_nofollow: bool,
    pub link_kind: String,
}

pub fn parse_fetched_page(
    page_url: &str,
    response: &FetchResponse,
    origin: &SiteOrigin,
) -> ParsedPage {
    let mut page = parse_html_page(page_url, response, origin);
    page.http_headers = json!(response.headers);
    page.technical_score = technical_score(response, &page);
    page.internal_links = page
        .links
        .iter()
        .map(|l| LinkEdge {
            target_url: l.target_url.clone(),
            anchor_text: l.anchor_text.clone(),
            rel: l.rel.clone(),
            is_nofollow: l.is_nofollow,
            link_kind: if l.target_url.starts_with(&origin.origin) {
                "internal".into()
            } else {
                "external".into()
            },
        })
        .collect();
    page
}

pub fn normalize_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn content_hash(text: &str) -> String {
    let normalized = normalize_whitespace(text);
    let digest = Sha256::digest(normalized.as_bytes());
    format!("sha256:{:x}", digest)
}

/// Heuristic: flag when any token appears more than `max_ratio` of word count.
pub fn repetitive_token_ratio(text: &str) -> f64 {
    let words: Vec<String> = text
        .to_lowercase()
        .split_whitespace()
        .filter(|w| w.len() >= 4)
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_string())
        .filter(|w| w.len() >= 4)
        .collect();
    if words.len() < 20 {
        return 0.0;
    }
    let mut counts = std::collections::HashMap::<&str, usize>::new();
    for w in &words {
        *counts.entry(w.as_str()).or_default() += 1;
    }
    counts
        .values()
        .copied()
        .max()
        .unwrap_or(0) as f64
        / words.len() as f64
}

pub fn heading_hierarchy_ok(outline: &[HeadingEntry]) -> bool {
    if outline.is_empty() {
        return true;
    }
    let mut prev = outline[0].level;
    for h in outline.iter().skip(1) {
        if h.level > prev + 1 {
            return false;
        }
        prev = h.level;
    }
    true
}

pub fn parse_html_page(url: &str, response: &FetchResponse, origin: &SiteOrigin) -> ParsedPage {
    let document = Html::parse_document(&response.body);
    let title = select_text(&document, "title");
    let meta_description = meta_content(&document, "description");
    let h1_texts = select_all_text(&document, "h1");
    let h1 = h1_texts.first().cloned();
    let h1_count = h1_texts.len() as u32;
    let heading_outline = extract_heading_outline(&document);
    let (img_count, img_missing_alt) = extract_image_alt_stats(&document);
    let head = extract_head_json(&document, &response.final_url, &title, &meta_description, &h1);
    let main_text = extract_main_text(&document);
    let hash = content_hash(&main_text);
    let links = extract_links(&document, url, origin);
    let structured_data_count = document
        .select(&Selector::parse("script[type=\"application/ld+json\"]").unwrap())
        .count() as u32;
    let has_faq_schema = document
        .select(&Selector::parse("script[type=\"application/ld+json\"]").unwrap())
        .any(|el| {
            el.text()
                .collect::<String>()
                .to_lowercase()
                .contains("faqpage")
        });
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
    if h1_count == 0 {
        issues.push(IssueRow {
            issue_code: "MISSING_H1".into(),
            message: "Page is missing H1".into(),
            severity: "medium".into(),
        });
    }
    let internal_hrefs = links
        .iter()
        .filter(|l| l.target_url.starts_with(&origin.origin))
        .count();
    let has_spa_root = document
        .select(&Selector::parse("#root, #app").unwrap())
        .next()
        .is_some();
    let has_module_script = document
        .select(&Selector::parse("script[type=\"module\"]").unwrap())
        .next()
        .is_some();
    if has_spa_root && has_module_script && internal_hrefs == 0 {
        issues.push(IssueRow {
            issue_code: "SPA_SHELL_NO_STATIC_LINKS".into(),
            message: "Page looks like a JS SPA shell with no crawlable internal links in static HTML; use seed_urls or site_quality for rendered metrics".into(),
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
        h1_count,
        heading_outline,
        img_count,
        img_missing_alt,
        head,
        http_headers: json!({}),
        links,
        internal_links: Vec::new(),
        main_text,
        content_hash: hash,
        structured_data_count,
        has_faq_schema,
        technical_score: 0.0,
        issues,
    }
}

pub fn technical_score(response: &FetchResponse, page: &ParsedPage) -> f64 {
    let mut score: f64 = 1.0;
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
    if page.img_count > 0 && page.img_missing_alt > 0 {
        score -= 0.05 * (page.img_missing_alt as f64 / page.img_count as f64);
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

fn select_all_text(document: &Html, selector: &str) -> Vec<String> {
    let sel = match Selector::parse(selector) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    document
        .select(&sel)
        .map(|el| el.text().collect::<String>().trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
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

fn extract_heading_outline(document: &Html) -> Vec<HeadingEntry> {
    let sel = match Selector::parse("h1, h2, h3, h4, h5, h6") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let level_re = Regex::new(r"^h([1-6])$").expect("regex");
    let mut out = Vec::new();
    for el in document.select(&sel) {
        let tag = el.value().name();
        let level = level_re
            .captures(tag)
            .and_then(|c| c.get(1))
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(2);
        let text = el.text().collect::<String>().trim().to_string();
        if !text.is_empty() {
            out.push(HeadingEntry { level, text });
        }
    }
    out
}

fn extract_image_alt_stats(document: &Html) -> (u32, u32) {
    let sel = match Selector::parse("img") {
        Ok(s) => s,
        Err(_) => return (0, 0),
    };
    let mut total = 0u32;
    let mut missing = 0u32;
    for img in document.select(&sel) {
        total += 1;
        let alt = img.value().attr("alt").unwrap_or("").trim();
        if alt.is_empty() {
            missing += 1;
        }
    }
    (total, missing)
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
        head.insert("title_length".into(), json!(t.chars().count()));
    }
    if let Some(d) = meta_description {
        head.insert("meta_description".into(), json!(d));
        head.insert("meta_description_length".into(), json!(d.chars().count()));
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
    for sel_str in ["main", "article", "[role=main]"] {
        if let Ok(sel) = Selector::parse(sel_str) {
            if let Some(el) = document.select(&sel).next() {
                return normalize_whitespace(&el.text().collect::<String>());
            }
        }
    }
    let body_sel = Selector::parse("body").ok();
    let Some(body_sel) = body_sel else {
        return String::new();
    };
    document
        .select(&body_sel)
        .next()
        .map(|el| normalize_whitespace(&el.text().collect::<String>()))
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
        <a href="/about">About</a><img src="/x.png" alt="logo"></body></html>"#;
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
        assert_eq!(parsed.img_count, 1);
        assert_eq!(parsed.img_missing_alt, 0);
    }

    #[test]
    fn heading_hierarchy_detects_skip() {
        let outline = vec![
            HeadingEntry {
                level: 1,
                text: "A".into(),
            },
            HeadingEntry {
                level: 4,
                text: "B".into(),
            },
        ];
        assert!(!heading_hierarchy_ok(&outline));
    }
}
