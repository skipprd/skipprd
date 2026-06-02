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
        .or_else(|_| std::env::var("SKIPPR_CONTENT_QUALITY_FIXTURE_DIR"))
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
