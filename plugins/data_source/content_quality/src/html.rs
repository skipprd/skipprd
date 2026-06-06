use regex::Regex;
use scraper::{Html, Selector};
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
    let main_text = extract_main_text(&document);
    let hash = content_hash(&main_text);
    let blocks = extract_content_blocks(page_url, &document);
    let page_type = infer_page_type(&document, &blocks);
    let word_count = main_text.split_whitespace().count() as u32;
    let canonical = select_attr(&document, "link[rel=\"canonical\"]", "href")
        .unwrap_or_else(|| response.final_url.clone());
    let links = extract_links(&document, page_url, origin);
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
            .get("helpfulness_score")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        trust += row
            .get("trust_score")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        ai += row
            .get("extractability_score")
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
        "extractability_score": 0.75,
        "answer_clarity_score": 0.7,
        "citation_worthiness_score": 0.6,
        "helpfulness_score": 0.72,
        "trust_score": 0.68,
        "is_self_contained": block.word_count >= 20,
        "suggested_query_intents": [],
        "missing_for_ai_citation": [],
        "evidence_quotes": [block.text.chars().take(80).collect::<String>()],
        "confidence": 0.8
    })
}

fn infer_page_type(document: &Html, blocks: &[ContentBlock]) -> String {
    let body = document.root_element();
    let html = body.html();
    let lower = html.to_lowercase();
    if lower.contains("faq") || blocks.iter().any(|b| b.block_type == "faq") {
        return "faq".into();
    }
    if lower.contains("documentation") || lower.contains("docs/") {
        return "doc".into();
    }
    "article".into()
}

pub fn extract_content_blocks(page_url: &str, document: &Html) -> Vec<ContentBlock> {
    let mut blocks = Vec::new();
    let mut ordinal = 0u32;

    if let Ok(p_sel) = Selector::parse("main p, article p, [role=main] p, body p") {
        for p in document.select(&p_sel).take(3) {
            let text = p.text().collect::<String>().trim().to_string();
            if text.split_whitespace().count() < 15 {
                continue;
            }
            let text_hash = content_hash(&text);
            blocks.push(ContentBlock {
                block_id: block_id_for(page_url, &["intro".into()], ordinal),
                block_type: "intro".into(),
                heading_path: vec!["Introduction".into()],
                text: text.clone(),
                text_hash,
                char_count: text.chars().count(),
                word_count: text.split_whitespace().count(),
                ordinal,
                has_list: false,
                has_table: false,
                has_citation: false,
                outbound_link_count: 0,
            });
            ordinal += 1;
            break;
        }
    }

    let heading_sel = Selector::parse("h1, h2, h3, h4, h5, h6").ok();
    let Some(heading_sel) = heading_sel else {
        return blocks;
    };
    for heading in document.select(&heading_sel) {
        let text = heading.text().collect::<String>().trim().to_string();
        if text.is_empty() {
            continue;
        }
        let block_type = if text.contains('?') || question_re().is_match(&text.to_lowercase()) {
            "direct_answer"
        } else if text.to_lowercase().contains("faq") {
            "faq"
        } else {
            "heading_section"
        };
        let heading_path = vec![text.clone()];
        let text_hash = content_hash(&text);
        blocks.push(ContentBlock {
            block_id: block_id_for(page_url, &heading_path, ordinal),
            block_type: block_type.into(),
            heading_path,
            text: text.clone(),
            text_hash,
            char_count: text.chars().count(),
            word_count: text.split_whitespace().count(),
            ordinal,
            has_list: false,
            has_table: false,
            has_citation: false,
            outbound_link_count: 0,
        });
        ordinal += 1;
    }
    blocks
}

pub fn block_id_for(page_url: &str, heading_path: &[String], ordinal: u32) -> String {
    let seed = format!("{page_url}|{}|{ordinal}", heading_path.join(" > "));
    content_hash(&seed)
}

fn question_re() -> Regex {
    Regex::new(r"^(how|what|why|when|where|who)\b").expect("regex")
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
