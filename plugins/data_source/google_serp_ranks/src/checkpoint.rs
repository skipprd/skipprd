use serde::{Deserialize, Serialize};
use skippr_runtime_sdk::plugins::cdc::{CheckpointAuthority, CheckpointEnvelope, CheckpointKind};
use skippr_runtime_sdk::plugins::SourceSyncContext;
use skippr_runtime_sdk::source_compat::load_checkpoint_payload;

pub const CHECKPOINT_PAYLOAD_VERSION: u32 = 1;
const KEY_PREFIX: &str = "google_serp_ranks:query";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QueryTerminalStatus {
    Completed,
    Blocked,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueryCheckpoint {
    pub run_date: String,
    pub status: QueryTerminalStatus,
}

pub fn query_checkpoint_key(keyword: &str, country: &str, language: &str, device: &str) -> String {
    format!("{KEY_PREFIX}:{keyword}:{country}:{language}:{device}")
}

pub fn load_query_checkpoint(
    ctx: &dyn SourceSyncContext,
    keyword: &str,
    country: &str,
    language: &str,
    device: &str,
) -> Option<QueryCheckpoint> {
    let key = query_checkpoint_key(keyword, country, language, device);
    load_checkpoint_payload::<QueryCheckpoint>(ctx, &key)
}

pub fn store_query_checkpoint(
    ctx: &dyn SourceSyncContext,
    keyword: &str,
    country: &str,
    language: &str,
    device: &str,
    payload: &QueryCheckpoint,
) -> Result<(), std::io::Error> {
    let key = query_checkpoint_key(keyword, country, language, device);
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

pub fn should_skip_query_today(
    checkpoint: Option<&QueryCheckpoint>,
    run_date: &str,
    force_refresh_today: bool,
) -> bool {
    if force_refresh_today {
        return false;
    }
    checkpoint
        .map(|cp| cp.run_date == run_date)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_when_same_run_date() {
        let cp = QueryCheckpoint {
            run_date: "2026-05-30".into(),
            status: QueryTerminalStatus::Completed,
        };
        assert!(should_skip_query_today(Some(&cp), "2026-05-30", false));
        assert!(!should_skip_query_today(Some(&cp), "2026-05-31", false));
        assert!(!should_skip_query_today(Some(&cp), "2026-05-30", true));
    }

    #[test]
    fn query_checkpoint_key_includes_locale() {
        let key = query_checkpoint_key("widgets", "uk", "en", "desktop");
        assert!(key.contains("widgets"));
        assert!(key.contains("uk"));
        assert!(key.contains("desktop"));
    }

    #[test]
    fn checkpoint_serde_roundtrip() {
        let cp = QueryCheckpoint {
            run_date: "2026-05-30".into(),
            status: QueryTerminalStatus::Blocked,
        };
        let json = serde_json::to_string(&cp).unwrap();
        let decoded: QueryCheckpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, cp);
    }
}
