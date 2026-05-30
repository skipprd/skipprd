use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use skippr_plugin_shared_api_source::OpenAiChatClient;

use crate::blocks::{BlockAiScores, ContentBlock};
use crate::fetch::fixture_path_for_url;
use crate::origin::url_hash;

#[async_trait]
pub trait BlockAnalyzer: Send + Sync {
    async fn analyze_blocks(
        &self,
        page_url: &str,
        page_title: Option<&str>,
        blocks: &[ContentBlock],
        model: &str,
        max_blocks: u32,
    ) -> Result<Vec<BlockAiScores>, std::io::Error>;
}

pub struct OpenAiBlockAnalyzer {
    client: OpenAiChatClient,
    fixture_dir: Option<String>,
}

impl OpenAiBlockAnalyzer {
    pub fn from_env() -> Result<Self, std::io::Error> {
        let client = OpenAiChatClient::from_env().map_err(std::io::Error::other)?;
        Ok(Self {
            client,
            fixture_dir: std::env::var("SKIPPR_SEO_CRAWL_FIXTURE_DIR")
                .ok()
                .filter(|d| !d.trim().is_empty()),
        })
    }

    pub fn with_client(client: OpenAiChatClient) -> Self {
        Self {
            client,
            fixture_dir: None,
        }
    }
}

#[async_trait]
impl BlockAnalyzer for OpenAiBlockAnalyzer {
    async fn analyze_blocks(
        &self,
        page_url: &str,
        page_title: Option<&str>,
        blocks: &[ContentBlock],
        model: &str,
        max_blocks: u32,
    ) -> Result<Vec<BlockAiScores>, std::io::Error> {
        if blocks.is_empty() {
            return Ok(Vec::new());
        }
        if let Some(dir) = &self.fixture_dir {
            if let Some(scores) = load_fixture_block_scores(dir, page_url)? {
                return Ok(scores);
            }
        }
        let capped: Vec<_> = blocks.iter().take(max_blocks as usize).collect();
        let system = "You score content blocks for AI answer extractability. Respond with JSON {\"blocks\":[{block_id, extractability_score, answer_clarity_score, citation_worthiness_score, helpfulness_score, trust_score, is_self_contained, suggested_query_intents, missing_for_ai_citation, evidence_quotes, confidence}]}";
        let user = json!({
            "page_url": page_url,
            "page_title": page_title,
            "blocks": capped.iter().map(|b| json!({
                "block_id": b.block_id,
                "block_type": b.block_type,
                "text": b.text,
            })).collect::<Vec<_>>(),
        });
        let response = self
            .client
            .chat_json_object(
                system,
                &serde_json::to_string(&user).map_err(std::io::Error::other)?,
                model,
                Duration::from_secs(120),
            )
            .await
            .map_err(std::io::Error::other)?;
        parse_block_scores_response(&response, blocks)
    }
}

fn load_fixture_block_scores(
    dir: &str,
    page_url: &str,
) -> Result<Option<Vec<BlockAiScores>>, std::io::Error> {
    let hash = url_hash(page_url);
    let path = format!("{}/pages/{hash}.blocks.json", dir.trim_end_matches('/'));
    if !std::path::Path::new(&path).exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path)?;
    let value: Value = serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
    let arr = value
        .get("blocks")
        .and_then(|b| b.as_array())
        .cloned()
        .unwrap_or_default();
    let mut scores = Vec::new();
    for item in arr {
        scores.push(parse_single_block_score(&item)?);
    }
    Ok(Some(scores))
}

fn parse_block_scores_response(
    response: &Value,
    blocks: &[ContentBlock],
) -> Result<Vec<BlockAiScores>, std::io::Error> {
    let arr = response
        .get("blocks")
        .and_then(|b| b.as_array())
        .cloned()
        .unwrap_or_default();
    if !arr.is_empty() {
        return arr.iter().map(parse_single_block_score).collect();
    }
    blocks
        .iter()
        .map(|_| Ok(BlockAiScores::default()))
        .collect()
}

fn parse_single_block_score(item: &Value) -> Result<BlockAiScores, std::io::Error> {
    Ok(BlockAiScores {
        extractability_score: item
            .get("extractability_score")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
        answer_clarity_score: item
            .get("answer_clarity_score")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
        citation_worthiness_score: item
            .get("citation_worthiness_score")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
        helpfulness_score: item
            .get("helpfulness_score")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
        trust_score: item.get("trust_score").and_then(|v| v.as_f64()).unwrap_or(0.0),
        is_self_contained: item
            .get("is_self_contained")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        suggested_query_intents: item
            .get("suggested_query_intents")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        missing_for_ai_citation: item
            .get("missing_for_ai_citation")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        evidence_quotes: item
            .get("evidence_quotes")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        confidence: item.get("confidence").and_then(|v| v.as_f64()).unwrap_or(0.0),
    })
}

pub struct CountingBlockAnalyzer {
    inner: Arc<dyn BlockAnalyzer>,
    pub calls: Arc<AtomicUsize>,
}

impl CountingBlockAnalyzer {
    pub fn wrap(inner: Arc<dyn BlockAnalyzer>) -> Self {
        Self {
            inner,
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[async_trait]
impl BlockAnalyzer for CountingBlockAnalyzer {
    async fn analyze_blocks(
        &self,
        page_url: &str,
        page_title: Option<&str>,
        blocks: &[ContentBlock],
        model: &str,
        max_blocks: u32,
    ) -> Result<Vec<BlockAiScores>, std::io::Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner
            .analyze_blocks(page_url, page_title, blocks, model, max_blocks)
            .await
    }
}

pub struct FixtureBlockAnalyzer {
    scores: Vec<BlockAiScores>,
}

impl FixtureBlockAnalyzer {
    pub fn new(scores: Vec<BlockAiScores>) -> Self {
        Self { scores }
    }
}

#[async_trait]
impl BlockAnalyzer for FixtureBlockAnalyzer {
    async fn analyze_blocks(
        &self,
        _page_url: &str,
        _page_title: Option<&str>,
        blocks: &[ContentBlock],
        _model: &str,
        _max_blocks: u32,
    ) -> Result<Vec<BlockAiScores>, std::io::Error> {
        Ok(blocks
            .iter()
            .enumerate()
            .map(|(i, _)| {
                self.scores
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| BlockAiScores {
                        extractability_score: 0.8,
                        helpfulness_score: 0.75,
                        trust_score: 0.7,
                        ..BlockAiScores::default()
                    })
            })
            .collect())
    }
}
