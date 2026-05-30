use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::html::{content_sha256, normalize_whitespace};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BlockType {
    HeadingSection,
    DirectAnswer,
    Faq,
    Summary,
    Table,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContentBlock {
    pub block_id: String,
    pub block_type: BlockType,
    pub heading_path: Vec<String>,
    pub text: String,
    pub char_count: usize,
    pub word_count: usize,
    pub text_hash: String,
    pub has_list: bool,
    pub has_table: bool,
    pub has_citation: bool,
    pub outbound_link_count: u32,
    pub ordinal: u32,
}

pub fn extract_content_blocks(page_url: &str, html: &str) -> Vec<ContentBlock> {
    use scraper::{Html, Selector};

    let document = Html::parse_document(html);
    let heading_sel = Selector::parse("h1, h2, h3, h4, h5, h6").unwrap();
    let mut blocks = Vec::new();
    let mut heading_path: Vec<(u8, String)> = Vec::new();
    let mut ordinal = 0u32;

    let body_sel = Selector::parse("main, article, body").unwrap();
    let root = document
        .select(&body_sel)
        .next()
        .or_else(|| document.select(&Selector::parse("body").unwrap()).next());

    let Some(root) = root else {
        return blocks;
    };

    for child in root.children() {
        let Some(element) = child.value().as_element() else {
            continue;
        };
        let tag = element.name();
        if tag.starts_with('h') && tag.len() == 2 {
            if let Ok(level) = tag[1..2].parse::<u8>() {
                let text = child.text().collect::<String>();
                let text = normalize_whitespace(&text);
                while heading_path.last().is_some_and(|(l, _)| *l >= level) {
                    heading_path.pop();
                }
                heading_path.push((level, text.clone()));
                continue;
            }
        }
        if matches!(tag, "p" | "ul" | "ol" | "table" | "details" | "dl") {
            let text = child.text().collect::<String>();
            let text = normalize_whitespace(&text);
            if text.len() < 40 {
                continue;
            }
            let path_strings: Vec<String> = heading_path.iter().map(|(_, t)| t.clone()).collect();
            let block_type = classify_block(tag, &path_strings, &text);
            let block_id = stable_block_id(page_url, &path_strings, ordinal);
            let text_hash = content_sha256(&text);
            let has_list = matches!(tag, "ul" | "ol");
            let has_table = tag == "table";
            let has_citation = text.contains("http://") || text.contains("https://");
            blocks.push(ContentBlock {
                block_id,
                block_type,
                heading_path: path_strings,
                char_count: text.chars().count(),
                word_count: text.split_whitespace().count(),
                text_hash,
                has_list,
                has_table,
                has_citation,
                outbound_link_count: 0,
                ordinal,
                text,
            });
            ordinal += 1;
        }
    }

    if blocks.is_empty() {
        let fallback = normalize_whitespace(&root.text().collect::<String>());
        if fallback.len() >= 80 {
            blocks.push(ContentBlock {
                block_id: stable_block_id(page_url, &[], 0),
                block_type: BlockType::Summary,
                heading_path: vec![],
                char_count: fallback.chars().count(),
                word_count: fallback.split_whitespace().count(),
                text_hash: content_sha256(&fallback),
                has_list: false,
                has_table: false,
                has_citation: false,
                outbound_link_count: 0,
                ordinal: 0,
                text: fallback,
            });
        }
    }
    blocks
}

fn classify_block(tag: &str, heading_path: &[String], text: &str) -> BlockType {
    if tag == "details" || tag == "dl" {
        return BlockType::Faq;
    }
    if tag == "table" {
        return BlockType::Table;
    }
    if let Some(last) = heading_path.last() {
        let lower = last.to_ascii_lowercase();
        if lower.contains('?')
            || lower.starts_with("how ")
            || lower.starts_with("what ")
            || lower.starts_with("why ")
        {
            return BlockType::DirectAnswer;
        }
    }
    if text.to_ascii_lowercase().contains("in summary") {
        return BlockType::Summary;
    }
    BlockType::HeadingSection
}

pub fn stable_block_id(page_url: &str, heading_path: &[String], ordinal: u32) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(page_url.as_bytes());
    for h in heading_path {
        hasher.update(h.as_bytes());
    }
    hasher.update(ordinal.to_le_bytes());
    format!("blk_{:x}", hasher.finalize())
}

pub fn block_to_row(
    site: &str,
    crawl_date: &str,
    run_id: &str,
    page_url: &str,
    block: &ContentBlock,
    ai: Option<&BlockAiScores>,
    content_unchanged: bool,
) -> Value {
    let mut row = json!({
        "site": site,
        "crawl_date": crawl_date,
        "run_id": run_id,
        "page_url": page_url,
        "block_id": block.block_id,
        "block_type": block.block_type,
        "heading_path": block.heading_path,
        "char_count": block.char_count,
        "word_count": block.word_count,
        "text_hash": block.text_hash,
        "has_list": block.has_list,
        "has_table": block.has_table,
        "has_citation": block.has_citation,
        "outbound_link_count": block.outbound_link_count,
        "ordinal": block.ordinal,
        "content_unchanged": content_unchanged,
    });
    if let Some(ai) = ai {
        row["extractability_score"] = json!(ai.extractability_score);
        row["answer_clarity_score"] = json!(ai.answer_clarity_score);
        row["citation_worthiness_score"] = json!(ai.citation_worthiness_score);
        row["helpfulness_score"] = json!(ai.helpfulness_score);
        row["trust_score"] = json!(ai.trust_score);
        row["is_self_contained"] = json!(ai.is_self_contained);
        row["suggested_query_intents"] = json!(ai.suggested_query_intents);
        row["missing_for_ai_citation"] = json!(ai.missing_for_ai_citation);
        row["evidence_quotes"] = json!(ai.evidence_quotes);
        row["confidence"] = json!(ai.confidence);
    }
    row
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BlockAiScores {
    pub extractability_score: f64,
    pub answer_clarity_score: f64,
    pub citation_worthiness_score: f64,
    pub helpfulness_score: f64,
    pub trust_score: f64,
    pub is_self_contained: bool,
    pub suggested_query_intents: Vec<String>,
    pub missing_for_ai_citation: Vec<String>,
    pub evidence_quotes: Vec<String>,
    pub confidence: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PageScoreRollup {
    pub content_quality_score: f64,
    pub eeat_proxy_score: f64,
    pub ai_readiness_score: f64,
    pub self_contained_pct: f64,
    pub blocks_analyzed: u32,
}

pub fn rollup_page_scores(block_scores: &[BlockAiScores], weights: &[usize]) -> PageScoreRollup {
    if block_scores.is_empty() {
        return PageScoreRollup::default();
    }
    let total_weight: usize = weights.iter().sum();
    let w = if total_weight == 0 { 1 } else { total_weight };
    let mut cq = 0.0;
    let mut eeat = 0.0;
    let mut ai = 0.0;
    let mut self_contained = 0u32;
    for (scores, weight) in block_scores.iter().zip(weights.iter()) {
        let wf = *weight as f64 / w as f64;
        cq += scores.helpfulness_score * wf;
        eeat += scores.trust_score * wf;
        ai += scores.extractability_score * wf;
        if scores.is_self_contained {
            self_contained += 1;
        }
    }
    PageScoreRollup {
        content_quality_score: cq,
        eeat_proxy_score: eeat,
        ai_readiness_score: ai,
        self_contained_pct: self_contained as f64 / block_scores.len() as f64,
        blocks_analyzed: block_scores.len() as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_blocks_from_headings() {
        let html = r#"<!DOCTYPE html><html><body><main>
        <h1>Guide</h1>
        <h2>What is SEO?</h2>
        <p>SEO is search engine optimization and it helps sites rank in organic results when done well.</p>
        <h2>How to improve crawlability?</h2>
        <p>Use clean URLs, sitemaps, and robots.txt rules that allow important paths for crawlers.</p>
        </main></body></html>"#;
        let blocks = extract_content_blocks("https://example.com/guide", html);
        assert!(!blocks.is_empty());
        assert!(blocks.iter().any(|b| matches!(b.block_type, BlockType::DirectAnswer)));
    }
}
