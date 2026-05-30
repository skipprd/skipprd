use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use skippr_plugin_shared_api_source::OpenAiChatClient;

use crate::html::{mock_block_analysis, ContentBlock};

const SYSTEM_PROMPT: &str =
    "You analyze SEO/AEO content blocks. Respond with JSON {\"blocks\":[...]} only.";

pub async fn analyze_blocks_batch(
    client: &OpenAiChatClient,
    model: &str,
    site: &str,
    page_url: &str,
    blocks: &[ContentBlock],
) -> Result<Vec<serde_json::Value>, std::io::Error> {
    if blocks.is_empty() {
        return Ok(Vec::new());
    }
    if std::env::var("SKIPPR_OPENAI_FIXTURE_DIR")
        .or_else(|_| std::env::var("SKIPPR_SEO_CRAWL_FIXTURE_DIR"))
        .map(|d| !d.trim().is_empty())
        .unwrap_or(false)
    {
        return Ok(blocks.iter().map(mock_block_analysis).collect());
    }
    let user = serde_json::json!({
        "site": site,
        "page_url": page_url,
        "blocks": blocks.iter().map(|b| serde_json::json!({
            "block_id": b.block_id,
            "block_type": b.block_type,
            "text": b.text,
        })).collect::<Vec<_>>(),
    });
    let response = client
        .chat_json_object(model, SYSTEM_PROMPT, &user.to_string())
        .await?;
    let arr = response
        .get("blocks")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if arr.len() == blocks.len() {
        return Ok(arr);
    }
    Ok(blocks.iter().map(mock_block_analysis).collect())
}

pub struct CountingOpenAiClient {
    pub inner: OpenAiChatClient,
    pub calls: Arc<AtomicUsize>,
}

impl CountingOpenAiClient {
    pub fn wrap(inner: OpenAiChatClient) -> Self {
        Self {
            inner,
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub async fn analyze_blocks_batch(
        &self,
        model: &str,
        site: &str,
        page_url: &str,
        blocks: &[ContentBlock],
    ) -> Result<Vec<serde_json::Value>, std::io::Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        analyze_blocks_batch(&self.inner, model, site, page_url, blocks).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn openai_fixture_returns_json() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        std::env::set_var("SKIPPR_OPENAI_FIXTURE_DIR", dir);
        let client = OpenAiChatClient::new("fixture-key", "https://api.openai.com/v1");
        let block = ContentBlock {
            block_id: "h1:0".into(),
            block_type: "heading".into(),
            heading_path: vec![],
            text: "Title".into(),
            text_hash: "sha256:1".into(),
            char_count: 5,
            word_count: 1,
            ordinal: 0,
            has_list: false,
            has_table: false,
            has_citation: false,
            outbound_link_count: 0,
        };
        let out = analyze_blocks_batch(
            &client,
            "gpt-4.1-mini",
            "https://example.com",
            "https://example.com/",
            &[block],
        )
        .await
        .expect("fixture openai");
        assert!(!out.is_empty());
        std::env::remove_var("SKIPPR_OPENAI_FIXTURE_DIR");
    }
}
