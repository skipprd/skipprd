use serde::Deserialize;
use serde_derive::Serialize;
use skippr_runtime_sdk::plugins::cdc::{CheckpointAuthority, CheckpointEnvelope, CheckpointKind};
use skippr_runtime_sdk::plugins::SourceSyncContext;
use skippr_runtime_sdk::source_compat::load_checkpoint_payload;

pub const CHECKPOINT_PAYLOAD_VERSION: u32 = 1;
const KEY_PREFIX: &str = "site_quality:page";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PageCheckpoint {
    pub render_hash: String,
    pub lcp_ms: Option<f64>,
    pub inp_ms: Option<f64>,
    pub cls: Option<f64>,
    pub lh_performance: Option<f64>,
    pub lh_accessibility: Option<f64>,
    pub lh_best_practices: Option<f64>,
    pub lh_seo: Option<f64>,
    pub axe_summary_hash: Option<String>,
}

pub fn checkpoint_key(canonical_url: &str, device_profile: &str) -> String {
    format!("{KEY_PREFIX}:{canonical_url}:{device_profile}")
}

pub fn load_page_checkpoint(
    ctx: &dyn SourceSyncContext,
    canonical_url: &str,
    device_profile: &str,
) -> Option<PageCheckpoint> {
    let key = checkpoint_key(canonical_url, device_profile);
    load_checkpoint_payload::<PageCheckpoint>(ctx, &key)
}

pub fn store_page_checkpoint(
    ctx: &dyn SourceSyncContext,
    canonical_url: &str,
    device_profile: &str,
    payload: &PageCheckpoint,
) -> Result<(), std::io::Error> {
    let key = checkpoint_key(canonical_url, device_profile);
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

pub fn should_skip_heavy_audits(
    skip_when_unchanged: bool,
    checkpoint: Option<&PageCheckpoint>,
    render_hash: &str,
) -> bool {
    if !skip_when_unchanged {
        return false;
    }
    checkpoint
        .map(|cp| cp.render_hash == render_hash)
        .unwrap_or(false)
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_skips_when_hash_matches() {
        let cp = PageCheckpoint {
            render_hash: "sha256:abc".into(),
            lcp_ms: None,
            inp_ms: None,
            cls: None,
            lh_performance: None,
            lh_accessibility: None,
            lh_best_practices: None,
            lh_seo: None,
            axe_summary_hash: None,
        };
        assert!(should_skip_heavy_audits(true, Some(&cp), "sha256:abc"));
        assert!(!should_skip_heavy_audits(true, Some(&cp), "sha256:other"));
    }
}
