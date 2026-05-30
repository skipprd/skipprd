use serde::{Deserialize, Serialize};
use skippr_plugin_shared_api_source::{CheckpointPayload, JsonCheckpoint};
use skippr_runtime_sdk::plugins::cdc::{CheckpointAuthority, CheckpointEnvelope, CheckpointKind};
use skippr_runtime_sdk::plugins::SourceSyncContext;
use skippr_runtime_sdk::source_compat::load_checkpoint_payload;

use crate::blocks::{BlockAiScores, PageScoreRollup};

pub const CHECKPOINT_PAYLOAD_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PageScoresCheckpoint {
    pub technical_score: f64,
    pub content_quality_score: f64,
    pub eeat_proxy_score: f64,
    pub ai_readiness_score: f64,
    pub risk_score: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StoredBlockCheckpoint {
    pub text_hash: String,
    pub scores: Option<BlockAiScores>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PageCheckpoint {
    pub content_hash: String,
    pub last_crawl_date: String,
    pub page_scores: PageScoresCheckpoint,
    pub block_hashes: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub block_scores: std::collections::HashMap<String, BlockAiScores>,
    pub openai_model: String,
}

impl CheckpointPayload for PageCheckpoint {
    const VERSION: u32 = CHECKPOINT_PAYLOAD_VERSION;
}

pub fn page_checkpoint_key(canonical_url: &str) -> String {
    format!("seo_crawl:page:{canonical_url}")
}

pub fn load_page_checkpoint(ctx: &dyn SourceSyncContext, url: &str) -> Option<PageCheckpoint> {
    load_checkpoint_payload::<PageCheckpoint>(ctx, &page_checkpoint_key(url))
}

pub fn store_page_checkpoint(
    ctx: &dyn SourceSyncContext,
    url: &str,
    checkpoint: &PageCheckpoint,
) -> Result<(), std::io::Error> {
    let envelope = CheckpointEnvelope::from_payload(
        CheckpointAuthority::AdvisoryHint,
        CheckpointKind::SourceResume,
        CHECKPOINT_PAYLOAD_VERSION,
        checkpoint,
    )
    .map_err(|e| std::io::Error::other(e.to_string()))?;
    ctx.store_checkpoint(&page_checkpoint_key(url), &envelope)
        .map_err(std::io::Error::other)
}

pub fn page_scores_from_rollup(technical: f64, rollup: &PageScoreRollup) -> PageScoresCheckpoint {
    PageScoresCheckpoint {
        technical_score: technical,
        content_quality_score: rollup.content_quality_score,
        eeat_proxy_score: rollup.eeat_proxy_score,
        ai_readiness_score: rollup.ai_readiness_score,
        risk_score: (1.0 - rollup.content_quality_score).clamp(0.0, 1.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use skippr_plugin_shared_api_source::JsonCheckpoint;

    #[test]
    fn checkpoint_roundtrip_bytes() {
        let cp = PageCheckpoint {
            content_hash: "sha256:abc".into(),
            last_crawl_date: "2026-05-29".into(),
            page_scores: PageScoresCheckpoint {
                technical_score: 0.9,
                content_quality_score: 0.7,
                eeat_proxy_score: 0.75,
                ai_readiness_score: 0.8,
                risk_score: 0.2,
            },
            block_hashes: [("blk_1".into(), "sha256:1".into())]
                .into_iter()
                .collect(),
            block_scores: Default::default(),
            openai_model: "gpt-4.1-mini".into(),
        };
        let wrapped = JsonCheckpoint::new(cp.clone());
        let bytes = wrapped.to_bytes().unwrap();
        let decoded: JsonCheckpoint<PageCheckpoint> = JsonCheckpoint::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.payload, cp);
    }
}
