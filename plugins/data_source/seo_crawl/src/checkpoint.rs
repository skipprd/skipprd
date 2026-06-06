use serde::Deserialize;
use serde_derive::Serialize;
use skippr_plugin_shared_api_source::CheckpointPayload;
use skippr_runtime_sdk::plugins::cdc::{CheckpointAuthority, CheckpointEnvelope, CheckpointKind};
use skippr_runtime_sdk::plugins::SourceSyncContext;
use skippr_runtime_sdk::source_compat::load_checkpoint_payload;

pub const CHECKPOINT_PAYLOAD_VERSION: u32 = 2;
const KEY_PREFIX: &str = "seo_crawl:page";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PageTechnicalCheckpoint {
    pub content_hash: String,
    pub last_crawl_date: String,
    pub technical_score: f64,
}

impl CheckpointPayload for PageTechnicalCheckpoint {
    const VERSION: u32 = CHECKPOINT_PAYLOAD_VERSION;
}

pub fn checkpoint_key(canonical_url: &str) -> String {
    format!("{KEY_PREFIX}:{canonical_url}")
}

pub fn load_page_checkpoint(
    ctx: &dyn SourceSyncContext,
    canonical_url: &str,
) -> Option<PageTechnicalCheckpoint> {
    load_checkpoint_payload::<PageTechnicalCheckpoint>(ctx, &checkpoint_key(canonical_url))
}

pub fn store_page_checkpoint(
    ctx: &dyn SourceSyncContext,
    canonical_url: &str,
    payload: &PageTechnicalCheckpoint,
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
    checkpoint: Option<&PageTechnicalCheckpoint>,
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
        let cp = PageTechnicalCheckpoint {
            content_hash: "sha256:abc".into(),
            last_crawl_date: "2026-05-29".into(),
            technical_score: 0.9,
        };
        assert!(content_unchanged(true, Some(&cp), "sha256:abc"));
        assert!(!content_unchanged(true, Some(&cp), "sha256:def"));
    }

    #[test]
    fn checkpoint_roundtrip_in_offset_store() {
        let cp = PageTechnicalCheckpoint {
            content_hash: "sha256:deadbeef".into(),
            last_crawl_date: "2026-05-29".into(),
            technical_score: 0.88,
        };
        let bytes = skippr_plugin_shared_api_source::JsonCheckpoint::new(cp.clone())
            .to_bytes()
            .unwrap();
        let decoded: skippr_plugin_shared_api_source::JsonCheckpoint<PageTechnicalCheckpoint> =
            skippr_plugin_shared_api_source::JsonCheckpoint::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.payload, cp);
    }
}
