use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use skippr_plugin_shared_api_source::OpenAiChatClient;

use crate::html::{mock_block_analysis, ContentBlock};

const SYSTEM_PROMPT: &str = r#"You analyze page content for search discoverability and AI citation readiness.
Respond with JSON {"blocks":[...]} only. Each block must include:
block_id, extractability_score, answer_clarity_score, citation_worthiness_score, helpfulness_score,
trust_score, search_helpfulness, ai_citation_likelihood, topical_depth, expertise_signals_present,
source_citations_present, is_self_contained, suggested_query_intents, missing_for_ai_citation,
evidence_quotes, confidence. Scores are 0..1. Use evidence_quotes for any warn/fail rationale."#;

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
            "heading_path": b.heading_path,
            "word_count": b.word_count,
            "has_list": b.has_list,
            "has_table": b.has_table,
            "has_citation": b.has_citation,
            "outbound_link_count": b.outbound_link_count,
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
        return Ok(arr
            .into_iter()
            .zip(blocks.iter())
            .map(|(mut row, block)| {
                normalize_analysis_row(&mut row, block, model);
                row
            })
            .collect());
    }
    Ok(blocks.iter().map(mock_block_analysis).collect())
}

fn normalize_analysis_row(row: &mut serde_json::Value, block: &ContentBlock, model: &str) {
    let helpfulness = row.get("helpfulness_score").cloned();
    let citation = row.get("citation_worthiness_score").cloned();
    let Some(obj) = row.as_object_mut() else {
        return;
    };
    obj.entry("block_id")
        .or_insert_with(|| serde_json::json!(block.block_id));
    obj.entry("analysis_source")
        .or_insert_with(|| serde_json::json!("llm"));
    obj.entry("analysis_model")
        .or_insert_with(|| serde_json::json!(model));
    obj.entry("prompt_version")
        .or_insert_with(|| serde_json::json!("content-quality-discoverability-v1"));
    obj.entry("analysis_status")
        .or_insert_with(|| serde_json::json!("scored"));
    obj.entry("analysis_error")
        .or_insert_with(|| serde_json::Value::Null);
    obj.entry("search_helpfulness")
        .or_insert_with(|| helpfulness.unwrap_or(serde_json::json!(0.0)));
    obj.entry("ai_citation_likelihood")
        .or_insert_with(|| citation.unwrap_or(serde_json::json!(0.0)));
    obj.entry("topical_depth").or_insert_with(|| {
        serde_json::json!(if block.word_count >= 120 {
            "deep"
        } else if block.word_count >= 50 {
            "moderate"
        } else {
            "shallow"
        })
    });
    obj.entry("expertise_signals_present")
        .or_insert_with(|| serde_json::json!(block.has_citation));
    obj.entry("source_citations_present")
        .or_insert_with(|| serde_json::json!(block.has_citation));
    obj.entry("suggested_query_intents")
        .or_insert_with(|| serde_json::json!([]));
    obj.entry("missing_for_ai_citation")
        .or_insert_with(|| serde_json::json!([]));
    obj.entry("evidence_quotes")
        .or_insert_with(|| serde_json::json!([block.text.chars().take(120).collect::<String>()]));
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
