use regex::Regex;
use scraper::{ElementRef, Html, Selector};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::checkpoint::PageScores;
use crate::fetch::FetchResponse;
use crate::origin::SiteOrigin;

#[derive(Debug, Clone)]
pub struct ContentBlock {
    pub block_id: String,
    pub block_type: String,
    pub heading_path: Vec<String>,
    pub text: String,
    pub text_hash: String,
    pub char_count: usize,
    pub word_count: usize,
    pub ordinal: u32,
    pub has_list: bool,
    pub has_table: bool,
    pub has_citation: bool,
    pub outbound_link_count: u32,
}

#[derive(Debug, Clone)]
pub struct LinkEdge {
    pub target_url: String,
    pub anchor_text: String,
    pub rel: Vec<String>,
    pub is_nofollow: bool,
    pub link_kind: String,
}

#[derive(Debug, Clone)]
pub struct ParsedLink {
    pub target_url: String,
    pub anchor_text: String,
    pub rel: Vec<String>,
    pub is_nofollow: bool,
}

#[derive(Debug, Clone)]
pub struct ParsedPage {
    pub canonical_url: String,
    pub title: Option<String>,
    pub page_type: String,
    pub blocks: Vec<ContentBlock>,
    pub main_text: String,
    pub content_hash: String,
    pub word_count: u32,
    pub extraction_method: String,
    pub extraction_failure_reason: Option<String>,
    pub has_author_signal: bool,
    pub has_publish_date: bool,
    pub has_about_or_contact_link: bool,
    pub outbound_citation_count: u32,
    pub has_faq_schema: bool,
    pub question_heading_count: u32,
    pub links: Vec<ParsedLink>,
    pub internal_links: Vec<LinkEdge>,
}

pub fn parse_fetched_page(
    page_url: &str,
    response: &FetchResponse,
    origin: &SiteOrigin,
) -> ParsedPage {
    let document = Html::parse_document(&response.body);
    let title = select_text(&document, "title");
    let blocks = extract_content_blocks(page_url, &document);
    let extraction_method = extraction_method(&response.body, &blocks);
    let extraction_failure_reason = extraction_failure_reason(&response.body, &blocks);
    let main_text = extract_main_text(&document, &blocks);
    let hash = content_hash(&main_text);
    let has_faq_schema = has_faq_schema(&document);
    let question_heading_count = blocks
        .iter()
        .filter(|b| b.block_type == "direct_answer")
        .count() as u32;
    let page_type = infer_page_type(page_url, &document, &blocks, has_faq_schema);
    let word_count = main_text.split_whitespace().count() as u32;
    let canonical = select_attr(&document, "link[rel=\"canonical\"]", "href")
        .unwrap_or_else(|| response.final_url.clone());
    let links = extract_links(&document, page_url, origin);
    let outbound_citation_count = links
        .iter()
        .filter(|l| !l.target_url.starts_with(&origin.origin))
        .count() as u32;
    let internal_links = links
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
    ParsedPage {
        canonical_url: canonical,
        title,
        page_type,
        blocks,
        main_text,
        content_hash: hash,
        word_count,
        extraction_method,
        extraction_failure_reason,
        has_author_signal: has_author_signal(&document),
        has_publish_date: has_publish_date(&document),
        has_about_or_contact_link: links.iter().any(|l| {
            let text = l.anchor_text.to_lowercase();
            text.contains("about") || text.contains("contact")
        }),
        outbound_citation_count,
        has_faq_schema,
        question_heading_count,
        links,
        internal_links,
    }
}

pub fn normalize_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn content_hash(text: &str) -> String {
    let normalized = normalize_whitespace(text);
    let digest = Sha256::digest(normalized.as_bytes());
    format!("sha256:{:x}", digest)
}

pub fn rollup_page_scores(block_scores: &[Value]) -> PageScores {
    if block_scores.is_empty() {
        return PageScores::default();
    }
    let n = block_scores.len() as f64;
    let mut helpfulness = 0.0;
    let mut trust = 0.0;
    let mut ai = 0.0;
    for row in block_scores {
        helpfulness += row
            .get("search_helpfulness")
            .or_else(|| row.get("helpfulness_score"))
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        trust += row
            .get("trust_score")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        ai += row
            .get("ai_citation_likelihood")
            .or_else(|| row.get("extractability_score"))
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
    }
    PageScores {
        seo_content_score: helpfulness / n,
        aio_score: ai / n,
        eeat_proxy_score: trust / n,
    }
}

pub fn mock_block_analysis(block: &ContentBlock) -> Value {
    json!({
        "block_id": block.block_id,
        "analysis_source": "heuristic_mock",
        "analysis_model": "mock",
        "prompt_version": "content-quality-discoverability-v1",
        "analysis_status": "mock",
        "analysis_error": null,
        "extractability_score": 0.75,
        "answer_clarity_score": 0.7,
        "citation_worthiness_score": 0.6,
        "helpfulness_score": 0.72,
        "trust_score": 0.68,
        "search_helpfulness": 0.72,
        "ai_citation_likelihood": 0.6,
        "topical_depth": if block.word_count >= 80 { "moderate" } else { "shallow" },
        "expertise_signals_present": block.has_citation,
        "source_citations_present": block.has_citation,
        "is_self_contained": block.word_count >= 20,
        "suggested_query_intents": [],
        "missing_for_ai_citation": [],
        "evidence_quotes": [block.text.chars().take(80).collect::<String>()],
        "confidence": 0.8
    })
}

fn infer_page_type(
    page_url: &str,
    document: &Html,
    blocks: &[ContentBlock],
    has_faq_schema: bool,
) -> String {
    let body = document.root_element();
    let html = body.html();
    let lower = html.to_lowercase();
    let lower_url = page_url.to_lowercase();
    if has_faq_schema
        || lower_url.contains("/faq")
        || lower.contains("faq")
        || blocks.iter().any(|b| b.block_type == "faq")
    {
        return "faq".into();
    }
    if lower_url.contains("/docs")
        || lower_url.contains("/documentation")
        || lower.contains("documentation")
        || lower.contains("docs/")
    {
        return "doc".into();
    }
    if lower_url.contains("/pricing") || lower_url.contains("/product") {
        return "commercial".into();
    }
    "article".into()
}

fn has_faq_schema(document: &Html) -> bool {
    let sel = match Selector::parse("script[type=\"application/ld+json\"]") {
        Ok(s) => s,
        Err(_) => return false,
    };
    document.select(&sel).any(|el| {
        el.text()
            .collect::<String>()
            .to_lowercase()
            .contains("faqpage")
    })
}

pub fn extract_content_blocks(page_url: &str, document: &Html) -> Vec<ContentBlock> {
    let mut blocks = Vec::new();
    let mut ordinal = 0u32;
    let mut heading_path: Vec<String> = Vec::new();

    if let Ok(flow_sel) = Selector::parse("h1, h2, h3, h4, h5, h6, p, li, td, blockquote") {
        for el in document.select(&flow_sel) {
            if is_chrome_element(&el) {
                continue;
            }
            let tag = el.value().name();
            let text = normalize_whitespace(&el.text().collect::<String>());
            if text.is_empty() {
                continue;
            }
            if tag.starts_with('h') && tag.len() == 2 {
                heading_path = vec![text.clone()];
                if is_question_text(&text) {
                    blocks.push(build_block(
                        page_url,
                        "direct_answer",
                        heading_path.clone(),
                        text,
                        ordinal,
                        &el,
                    ));
                    ordinal += 1;
                }
                continue;
            }
            if text.split_whitespace().count() < 12 {
                continue;
            }
            let block_type = if heading_path.last().is_some_and(|h| is_question_text(h))
                || is_question_text(&text)
            {
                "direct_answer"
            } else if heading_path
                .last()
                .is_some_and(|h| h.to_lowercase().contains("faq"))
            {
                "faq"
            } else if tag == "blockquote" {
                "citation"
            } else {
                "heading_section"
            };
            let path = if heading_path.is_empty() {
                vec!["Primary content".into()]
            } else {
                heading_path.clone()
            };
            blocks.push(build_block(page_url, block_type, path, text, ordinal, &el));
            ordinal += 1;
        }
    }

    if !blocks.is_empty() {
        return dedupe_blocks(blocks);
    }

    let Ok(region_sel) = Selector::parse("article, main, section, div") else {
        return blocks;
    };
    for el in document.select(&region_sel) {
        if is_chrome_element(&el) {
            continue;
        }
        let text = normalize_whitespace(&el.text().collect::<String>());
        let words = text.split_whitespace().count();
        if words < 40 || link_ratio(&el) > 0.35 {
            continue;
        }
        let heading_path = first_heading_text(&el)
            .map(|h| vec![h])
            .unwrap_or_else(|| vec!["Primary content".into()]);
        blocks.push(build_block(
            page_url,
            "primary_region",
            heading_path,
            text,
            ordinal,
            &el,
        ));
        ordinal += 1;
        if ordinal >= 5 {
            break;
        }
    }
    dedupe_blocks(blocks)
}

pub fn block_id_for(page_url: &str, heading_path: &[String], ordinal: u32) -> String {
    let seed = format!("{page_url}|{}|{ordinal}", heading_path.join(" > "));
    content_hash(&seed)
}

fn question_re() -> Regex {
    Regex::new(r"^(how|what|why|when|where|who)\b").expect("regex")
}

fn is_question_text(text: &str) -> bool {
    text.contains('?') || question_re().is_match(&text.to_lowercase())
}

fn build_block(
    page_url: &str,
    block_type: &str,
    heading_path: Vec<String>,
    text: String,
    ordinal: u32,
    el: &ElementRef<'_>,
) -> ContentBlock {
    let outbound_link_count = count_links(el);
    ContentBlock {
        block_id: block_id_for(page_url, &heading_path, ordinal),
        block_type: block_type.into(),
        heading_path,
        text_hash: content_hash(&text),
        char_count: text.chars().count(),
        word_count: text.split_whitespace().count(),
        text,
        ordinal,
        has_list: has_descendant(el, "ul, ol, li"),
        has_table: has_descendant(el, "table, tr, td, th"),
        has_citation: outbound_link_count > 0 || has_descendant(el, "cite, blockquote"),
        outbound_link_count,
    }
}

fn dedupe_blocks(blocks: Vec<ContentBlock>) -> Vec<ContentBlock> {
    let mut seen = std::collections::HashSet::new();
    blocks
        .into_iter()
        .filter(|b| seen.insert(b.text_hash.clone()))
        .take(24)
        .collect()
}

fn has_descendant(el: &ElementRef<'_>, selector: &str) -> bool {
    Selector::parse(selector)
        .ok()
        .is_some_and(|sel| el.select(&sel).next().is_some())
}

fn count_links(el: &ElementRef<'_>) -> u32 {
    Selector::parse("a[href]")
        .ok()
        .map(|sel| el.select(&sel).count() as u32)
        .unwrap_or(0)
}

fn link_ratio(el: &ElementRef<'_>) -> f64 {
    let total = normalize_whitespace(&el.text().collect::<String>())
        .split_whitespace()
        .count();
    if total == 0 {
        return 1.0;
    }
    let link_words = Selector::parse("a[href]")
        .ok()
        .map(|sel| {
            el.select(&sel)
                .map(|a| {
                    normalize_whitespace(&a.text().collect::<String>())
                        .split_whitespace()
                        .count()
                })
                .sum::<usize>()
        })
        .unwrap_or(0);
    link_words as f64 / total as f64
}

fn first_heading_text(el: &ElementRef<'_>) -> Option<String> {
    let sel = Selector::parse("h1, h2, h3, h4, h5, h6").ok()?;
    el.select(&sel)
        .next()
        .map(|h| normalize_whitespace(&h.text().collect::<String>()))
        .filter(|s| !s.is_empty())
}

fn is_chrome_element(el: &ElementRef<'_>) -> bool {
    let tag = el.value().name();
    if matches!(
        tag,
        "script" | "style" | "noscript" | "nav" | "header" | "footer" | "aside"
    ) {
        return true;
    }
    let attrs = ["id", "class", "role", "aria-label"]
        .iter()
        .filter_map(|name| el.value().attr(name))
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    Regex::new(r"(nav|menu|footer|header|cookie|consent|banner|modal|sidebar|breadcrumb)")
        .expect("regex")
        .is_match(&attrs)
}

fn extraction_method(html: &str, blocks: &[ContentBlock]) -> String {
    if !blocks.is_empty() {
        "static_ok".into()
    } else if looks_like_js_shell(html) {
        "js_shell".into()
    } else {
        "empty".into()
    }
}

fn extraction_failure_reason(html: &str, blocks: &[ContentBlock]) -> Option<String> {
    if !blocks.is_empty() {
        None
    } else if looks_like_js_shell(html) {
        Some("js_shell".into())
    } else {
        Some("empty".into())
    }
}

fn looks_like_js_shell(html: &str) -> bool {
    let lower = html.to_lowercase();
    let script_count = lower.matches("<script").count();
    let document = Html::parse_document(html);
    let body_words = cleaned_body_text(&document).split_whitespace().count();
    script_count >= 2 && body_words < 40
}

fn has_author_signal(document: &Html) -> bool {
    select_attr(document, "a[rel=\"author\"]", "href").is_some()
        || select_text(
            document,
            "[class*=\"author\"], [id*=\"author\"], [itemprop=\"author\"]",
        )
        .is_some()
}

fn has_publish_date(document: &Html) -> bool {
    select_attr(
        document,
        "time[datetime], meta[property=\"article:published_time\"], meta[name=\"date\"]",
        "datetime",
    )
    .or_else(|| {
        select_attr(
            document,
            "meta[property=\"article:published_time\"], meta[name=\"date\"]",
            "content",
        )
    })
    .is_some()
}

fn select_text(document: &Html, selector: &str) -> Option<String> {
    let sel = Selector::parse(selector).ok()?;
    document
        .select(&sel)
        .next()
        .map(|el| el.text().collect::<String>().trim().to_string())
        .filter(|s| !s.is_empty())
}

fn select_attr(document: &Html, selector: &str, attr: &str) -> Option<String> {
    let sel = Selector::parse(selector).ok()?;
    document
        .select(&sel)
        .next()
        .and_then(|el| el.value().attr(attr))
        .map(str::to_string)
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
        let Some(target_url) = crate::fetch::resolve_href(href, page_url, origin) else {
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

fn extract_main_text(document: &Html, blocks: &[ContentBlock]) -> String {
    if !blocks.is_empty() {
        return normalize_whitespace(
            &blocks
                .iter()
                .map(|b| b.text.as_str())
                .collect::<Vec<_>>()
                .join("\n\n"),
        );
    }
    cleaned_body_text(document)
}

fn cleaned_body_text(document: &Html) -> String {
    let body_sel = Selector::parse("body").ok();
    let Some(body_sel) = body_sel else {
        return String::new();
    };
    let Some(body) = document.select(&body_sel).next() else {
        return String::new();
    };
    let mut parts = Vec::new();
    if let Ok(sel) = Selector::parse("p, li, td, blockquote, h1, h2, h3, h4, h5, h6") {
        for el in body.select(&sel) {
            if is_chrome_element(&el) {
                continue;
            }
            let text = normalize_whitespace(&el.text().collect::<String>());
            if !text.is_empty() {
                parts.push(text);
            }
        }
    }
    normalize_whitespace(&parts.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crawler::seed_origin;
    use crate::fetch::FetchResponse;
    use std::collections::HashMap;

    #[test]
    fn parse_page_extracts_aio_evidence() {
        let origin = seed_origin("https://example.com").expect("origin");
        let response = FetchResponse {
            final_url: "https://example.com/faq".into(),
            status: 200,
            headers: HashMap::new(),
            body: r#"
              <html>
                <head>
                  <title>FAQ</title>
                  <script type="application/ld+json">{"@type":"FAQPage"}</script>
                </head>
                <body>
                  <main>
                    <p>This introductory paragraph has enough words to be extracted as content for the page.</p>
                    <h1>What is Skippr?</h1>
                  </main>
                </body>
              </html>
            "#.into(),
            redirect_chain: vec![200],
            ttfb_ms: 1,
        };
        let page = parse_fetched_page("https://example.com/faq", &response, &origin);
        assert!(page.has_faq_schema);
        assert_eq!(page.question_heading_count, 1);
        assert_eq!(page.page_type, "faq");
    }

    #[test]
    fn parse_page_ignores_static_js_shell_text() {
        let origin = seed_origin("https://example.com").expect("origin");
        let response = FetchResponse {
            final_url: "https://example.com/blog".into(),
            status: 200,
            headers: HashMap::new(),
            body: r#"
              <html>
                <head><title>Shell</title><script src="/app.js"></script><script>window.__app={}</script></head>
                <body><div id="app"></div><noscript>Please enable JavaScript to continue.</noscript></body>
              </html>
            "#.into(),
            redirect_chain: vec![200],
            ttfb_ms: 1,
        };
        let page = parse_fetched_page("https://example.com/blog", &response, &origin);
        assert!(page.blocks.is_empty());
        assert_eq!(page.word_count, 0);
        assert_eq!(page.extraction_method, "js_shell");
    }

    #[test]
    fn parse_page_extracts_div_heavy_content() {
        let origin = seed_origin("https://example.com").expect("origin");
        let response = FetchResponse {
            final_url: "https://example.com/guide".into(),
            status: 200,
            headers: HashMap::new(),
            body: r#"
              <html><body>
                <div class="top-nav">Home Pricing Login</div>
                <div class="cms">
                  <h1>How does content quality work?</h1>
                  <div>This guide explains content quality signals with enough detail to help search engines and AI systems understand the answer, trust the evidence, and cite the page confidently.</div>
                  <p>Author: Upfoundry research team. Updated June 2026 with references to current indexing and answer extraction behaviour.</p>
                </div>
              </body></html>
            "#.into(),
            redirect_chain: vec![200],
            ttfb_ms: 1,
        };
        let page = parse_fetched_page("https://example.com/guide", &response, &origin);
        assert!(!page.blocks.is_empty());
        assert!(page.word_count > 20);
        assert_eq!(page.extraction_method, "static_ok");
    }
}
