use serde_json::{json, Value};

use crate::html::{normalize_whitespace, ParsedPage};

#[derive(Clone, Debug)]
pub struct ContentBlock {
    pub block_id: String,
    pub block_type: String,
    pub heading: Option<String>,
    pub text: String,
    pub block_hash: String,
}

pub fn extract_content_blocks(page: &ParsedPage) -> Vec<ContentBlock> {
    let mut blocks = Vec::new();
    if let Some(h1) = page.h1.as_ref().filter(|t| !t.is_empty()) {
        blocks.push(ContentBlock {
            block_id: "h1:0".into(),
            block_type: "heading".into(),
            heading: Some(h1.clone()),
            text: h1.clone(),
            block_hash: block_sha256(h1),
        });
    }
    let main = normalize_whitespace(&page.main_text);
    if main.len() > 80 {
        blocks.push(ContentBlock {
            block_id: "main:0".into(),
            block_type: "main_text".into(),
            heading: None,
            text: main.clone(),
            block_hash: block_sha256(&main),
        });
    }
    blocks
}

pub fn block_sha256(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(text.as_bytes());
    format!("sha256:{:x}", digest)
}

pub fn block_row(
    site: &str,
    crawl_date: &str,
    page_url: &str,
    block: &ContentBlock,
    openai: Option<&Value>,
    content_unchanged: bool,
) -> Value {
    json!({
        "site": site,
        "crawl_date": crawl_date,
        "page_url": page_url,
        "block_id": block.block_id,
        "block_type": block.block_type,
        "heading": block.heading,
        "text": block.text,
        "block_hash": block.block_hash,
        "content_unchanged": content_unchanged,
        "openai_analysis": openai,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html::ParsedPage;

    #[test]
    fn extracts_h1_and_main_blocks() {
        let page = ParsedPage {
            canonical_url: "https://example.com/".into(),
            h1: Some("Hello".into()),
            head: json!({}),
            links: vec![],
            main_text: "x".repeat(100),
            content_hash: "sha256:1".into(),
            structured_data_count: 0,
            issues: vec![],
        };
        let blocks = extract_content_blocks(&page);
        assert!(blocks.iter().any(|b| b.block_type == "heading"));
        assert!(blocks.iter().any(|b| b.block_type == "main_text"));
    }
}
