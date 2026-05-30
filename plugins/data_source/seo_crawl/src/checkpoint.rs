use std::collections::HashMap;

use serde::Deserialize;
use serde_derive::Serialize;
use skippr_runtime_sdk::plugins::cdc::{CheckpointAuthority, CheckpointEnvelope, CheckpointKind};
use skippr_runtime_sdk::plugins::SourceSyncContext;
use skippr_runtime_sdk::source_compat::load_checkpoint_payload;

pub const CHECKPOINT_PAYLOAD_VERSION: u32 = 1;
const KEY_PREFIX: &str = "seo_crawl:page";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PageScores {
    pub technical_score: f64,
    pub content_quality_score: f64,
    pub eeat_proxy_score: f64,
    pub ai_readiness_score: f64,
    pub risk_score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PageContentCheckpoint {
    pub content_hash: String,
    pub last_crawl_date: String,
    pub page_scores: PageScores,
    pub block_hashes: HashMap<String, String>,
    #[serde(default)]
    pub block_scores: HashMap<String, serde_json::Value>,
    pub openai_model: String,
}

pub fn checkpoint_key(canonical_url: &str) -> String {
    format!("{KEY_PREFIX}:{canonical_url}")
}

pub fn load_page_checkpoint(
    ctx: &dyn SourceSyncContext,
    canonical_url: &str,
) -> Option<PageContentCheckpoint> {
    load_checkpoint_payload::<PageContentCheckpoint>(ctx, &checkpoint_key(canonical_url))
}

pub fn store_page_checkpoint(
    ctx: &dyn SourceSyncContext,
    canonical_url: &str,
    payload: &PageContentCheckpoint,
) -> Result<(), std::io::Error> {
    let envelope = CheckpointEnvelope::from_payload(
        CheckpointAuthority::AdvisoryHint,
        CheckpointKind::SourceResume,
        CHECKPOINT_PAYLOAD_VERSION,
        payload,
    )
    .map_err(|e| std::io::Error::other(e.to_string()))?;
    ctx.store_checkpoint(&checkpoint_key(canonical_url), &envelope)
        .map_err(std::io::Error::other)
}

pub fn content_unchanged(
    skip_when_unchanged: bool,
    checkpoint: Option<&PageContentCheckpoint>,
    content_hash: &str,
) -> bool {
    if !skip_when_unchanged {
        return false;
    }
    checkpoint
        .map(|cp| cp.content_hash == content_hash)
        .unwrap_or(false)
}

pub fn blocks_needing_analysis<'a>(
    blocks: &'a [crate::html::ContentBlock],
    checkpoint: Option<&PageContentCheckpoint>,
) -> Vec<&'a crate::html::ContentBlock> {
    let Some(cp) = checkpoint else {
        return blocks.iter().collect();
    };
    blocks
        .iter()
        .filter(|b| {
            cp.block_hashes
                .get(&b.block_id)
                .map(|h| h != &b.text_hash)
                .unwrap_or(true)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_unchanged_when_hash_matches() {
        let cp = PageContentCheckpoint {
            content_hash: "sha256:abc".into(),
            last_crawl_date: "2026-05-29".into(),
            page_scores: PageScores::default(),
            block_hashes: HashMap::new(),
            block_scores: HashMap::new(),
            openai_model: "gpt-4.1-mini".into(),
        };
        assert!(content_unchanged(true, Some(&cp), "sha256:abc"));
        assert!(!content_unchanged(true, Some(&cp), "sha256:def"));
    }

    #[test]
    fn checkpoint_roundtrip_in_offset_store() {
        let cp = PageContentCheckpoint {
            content_hash: "sha256:deadbeef".into(),
            last_crawl_date: "2026-05-29".into(),
            page_scores: PageScores {
                technical_score: 0.9,
                content_quality_score: 0.7,
                eeat_proxy_score: 0.71,
                ai_readiness_score: 0.82,
                risk_score: 0.18,
            },
            block_hashes: HashMap::from([("blk1".into(), "sha256:1".into())]),
            block_scores: HashMap::new(),
            openai_model: "gpt-4.1-mini".into(),
        };
        let bytes =
            skippr_plugin_shared_api_source::JsonCheckpoint::new(cp.clone()).to_bytes().unwrap();
        let decoded: skippr_plugin_shared_api_source::JsonCheckpoint<PageContentCheckpoint> =
            skippr_plugin_shared_api_source::JsonCheckpoint::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.payload, cp);
    }
}
