use serde::Deserialize;
use serde_derive::Serialize;
use skippr_runtime_sdk::plugins::cdc::{CheckpointAuthority, CheckpointEnvelope, CheckpointKind};
use skippr_runtime_sdk::plugins::SourceSyncContext;
use skippr_runtime_sdk::source_compat::load_checkpoint_payload;

pub const CHECKPOINT_PAYLOAD_VERSION: u32 = 1;
const KEY_PREFIX: &str = "ai_citations:prompt";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PromptCheckpoint {
    pub response_hash: String,
    pub brand_mentioned: bool,
    pub target_domain_linked: bool,
    pub mention_count: u32,
    pub citation_count: u32,
    pub link_count: u32,
}

pub fn checkpoint_key(prompt_id: &str, model: &str) -> String {
    format!("{KEY_PREFIX}:{prompt_id}:{model}")
}

pub fn load_prompt_checkpoint(
    ctx: &dyn SourceSyncContext,
    prompt_id: &str,
    model: &str,
) -> Option<PromptCheckpoint> {
    let key = checkpoint_key(prompt_id, model);
    load_checkpoint_payload::<PromptCheckpoint>(ctx, &key)
}

pub fn store_prompt_checkpoint(
    ctx: &dyn SourceSyncContext,
    prompt_id: &str,
    model: &str,
    payload: &PromptCheckpoint,
) -> Result<(), std::io::Error> {
    let key = checkpoint_key(prompt_id, model);
    let envelope = CheckpointEnvelope::from_payload(
        CheckpointAuthority::AdvisoryHint,
        CheckpointKind::SourceResume,
        CHECKPOINT_PAYLOAD_VERSION,
        payload,
    )
    .map_err(|e| std::io::Error::other(e.to_string()))?;
    ctx.store_checkpoint(&key, &envelope)
        .map_err(std::io::Error::other)
}

pub fn should_skip_unchanged(
    skip_when_unchanged: bool,
    checkpoint: Option<&PromptCheckpoint>,
    response_hash: &str,
) -> bool {
    if !skip_when_unchanged {
        return false;
    }
    checkpoint
        .map(|cp| cp.response_hash == response_hash)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::test_support::RecordingSyncContext;

    #[test]
    fn skips_when_hash_matches() {
        let cp = PromptCheckpoint {
            response_hash: "sha256:abc".into(),
            brand_mentioned: true,
            target_domain_linked: false,
            mention_count: 1,
            citation_count: 0,
            link_count: 2,
        };
        assert!(should_skip_unchanged(true, Some(&cp), "sha256:abc"));
        assert!(!should_skip_unchanged(true, Some(&cp), "sha256:other"));
    }

    #[test]
    fn checkpoint_roundtrip() {
        let ctx = Arc::new(RecordingSyncContext::default());
        let payload = PromptCheckpoint {
            response_hash: "sha256:deadbeef".into(),
            brand_mentioned: true,
            target_domain_linked: true,
            mention_count: 2,
            citation_count: 1,
            link_count: 3,
        };
        store_prompt_checkpoint(ctx.as_ref(), "p1", "gpt-4.1-mini", &payload).unwrap();
        let loaded = load_prompt_checkpoint(ctx.as_ref(), "p1", "gpt-4.1-mini").unwrap();
        assert_eq!(loaded, payload);
    }

    #[test]
    fn checkpoint_key_format() {
        assert_eq!(
            checkpoint_key("best_tools", "gpt-4.1-mini"),
            "ai_citations:prompt:best_tools:gpt-4.1-mini"
        );
    }
}
