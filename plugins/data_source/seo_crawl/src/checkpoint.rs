use serde::Deserialize;
use serde_derive::Serialize;
use skippr_runtime_sdk::plugins::cdc::{CheckpointAuthority, CheckpointEnvelope, CheckpointKind};
use skippr_runtime_sdk::plugins::SourceSyncContext;
use skippr_runtime_sdk::source_compat::load_checkpoint_payload;

pub const CHECKPOINT_PAYLOAD_VERSION: u32 = 1;
const KEY_PREFIX: &str = "seo_crawl:page";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PageContentCheckpoint {
    pub content_hash: String,
    pub block_hashes: Vec<String>,
}

pub fn checkpoint_key(canonical_url: &str) -> String {
    format!("{KEY_PREFIX}:{}", canonical_url)
}

pub fn load_page_checkpoint(ctx: &dyn SourceSyncContext, canonical_url: &str) -> Option<PageContentCheckpoint> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_unchanged_when_hash_matches() {
        let cp = PageContentCheckpoint {
            content_hash: "sha256:abc".into(),
            block_hashes: vec![],
        };
        assert!(content_unchanged(true, Some(&cp), "sha256:abc"));
        assert!(!content_unchanged(true, Some(&cp), "sha256:def"));
        assert!(!content_unchanged(false, Some(&cp), "sha256:abc"));
    }
}
