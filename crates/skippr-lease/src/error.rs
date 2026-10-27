use crate::identity::NodeId;
use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum LeaseError {
    #[error("lease held by {0}")]
    Held(NodeId),
    #[error("lease lost")]
    Lost,
    #[error("conditional lease update raced")]
    ConditionalRace,
    #[error("lease store unavailable: {0}")]
    StoreUnavailable(String),
    #[error("protocol mismatch: {0}")]
    ProtocolMismatch(String),
    #[error("fenced")]
    Fenced,
    #[error("quorum lost: {0}")]
    QuorumLost(String),
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum FenceError {
    #[error("pipeline is not the active primary")]
    NotActive,
    #[error("lease epoch mismatch")]
    EpochMismatch,
    #[error("local lease deadline expired")]
    DeadlineExpired,
    #[error("pipeline fenced")]
    Fenced,
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum PathError {
    #[error("invalid path component: {0}")]
    InvalidComponent(String),
    #[error("{0}")]
    Overflow(String),
}

/// Hard cap on `DurableError` `Display` for stringly details. Import/inventory
/// must never format thousands of paths into a log line.
pub const DURABLE_ERROR_DISPLAY_CEILING: usize = 512;

fn clip_error_detail(detail: &str) -> String {
    if detail.len() <= DURABLE_ERROR_DISPLAY_CEILING {
        return detail.to_string();
    }
    format!(
        "{}… (truncated, {} bytes)",
        &detail[..DURABLE_ERROR_DISPLAY_CEILING],
        detail.len()
    )
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum DurableError {
    #[error("fenced")]
    Fenced,
    #[error("fenced after commit")]
    FencedAfterCommit,
    #[error("quorum lost: {display}", display = clip_error_detail(.0))]
    QuorumLost(String),
    #[error("corrupt offset: {display}", display = clip_error_detail(.0))]
    CorruptOffset(String),
    #[error("io: {display}", display = clip_error_detail(.0))]
    Io(String),
    #[error("protocol mismatch: {display}", display = clip_error_detail(.0))]
    ProtocolMismatch(String),
    #[error("stale epoch")]
    StaleEpoch,
    #[error("not caught up")]
    NotCaughtUp,
    #[error("disk full")]
    DiskFull,
    #[error("replica RPC timeout")]
    Timeout,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublishMode {
    Fresh,
    Republish,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublishAction {
    Commit,
    AlreadyApplied,
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum OffsetPublishError {
    #[error("corrupt offset payload: {0}")]
    Corrupt(String),
    #[error("transient offset publish error: {0}")]
    Transient(String),
    #[error("offset fence regression")]
    FenceRegression,
}

/// Shared fence tuple compare for sled sibling keys and DynamoDB attributes.
pub fn decide_wal_publish(
    stored: Option<(u64, u64, &str)>,
    epoch: u64,
    seq: u64,
    payload_sha256: &str,
    mode: PublishMode,
) -> Result<PublishAction, OffsetPublishError> {
    let Some((old_epoch, old_seq, old_hash)) = stored else {
        return Ok(PublishAction::Commit);
    };
    if (old_epoch, old_seq) == (epoch, seq) {
        if old_hash == payload_sha256 {
            return Ok(PublishAction::AlreadyApplied);
        }
        return Err(OffsetPublishError::Corrupt(
            "same WAL tuple with different hash".into(),
        ));
    }
    if (old_epoch, old_seq) > (epoch, seq) {
        return match mode {
            PublishMode::Republish => Ok(PublishAction::AlreadyApplied),
            PublishMode::Fresh => Err(OffsetPublishError::FenceRegression),
        };
    }
    Ok(PublishAction::Commit)
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum PromoteError {
    #[error("lease error: {0}")]
    Lease(LeaseError),
    #[error("payload missing for segments: {segment_ids:?}")]
    PayloadMissing { segment_ids: Vec<String> },
    #[error("required schema missing: {0}")]
    SchemaMissing(String),
    #[error("replica assignment failed: {0}")]
    Replica(String),
    #[error("{0}")]
    Other(String),
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum ReplicaNack {
    #[error("stale epoch")]
    StaleEpoch,
    #[error("not assigned")]
    NotAssigned,
    #[error("not caught up")]
    NotCaughtUp,
    #[error("disk full")]
    DiskFull,
    #[error("payload hash mismatch")]
    PayloadHashMismatch,
    #[error("unknown pipeline")]
    UnknownPipeline,
    #[error("invalid primary endpoint")]
    InvalidPrimaryEndpoint,
}

impl ReplicaNack {
    pub fn into_durable(self) -> DurableError {
        match self {
            Self::StaleEpoch => DurableError::StaleEpoch,
            Self::NotAssigned | Self::NotCaughtUp => DurableError::NotCaughtUp,
            Self::DiskFull => DurableError::DiskFull,
            Self::PayloadHashMismatch | Self::UnknownPipeline | Self::InvalidPrimaryEndpoint => {
                DurableError::ProtocolMismatch(self.to_string())
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum ShutdownError {
    #[error("drain timed out")]
    DrainTimeout,
    #[error("{0}")]
    Other(String),
}

impl From<std::io::Error> for DurableError {
    fn from(err: std::io::Error) -> Self {
        if is_enospc(&err) {
            Self::DiskFull
        } else {
            Self::Io(clip_error_detail(&err.to_string()))
        }
    }
}

pub fn is_enospc(err: &std::io::Error) -> bool {
    matches!(err.kind(), std::io::ErrorKind::StorageFull)
        || matches!(err.raw_os_error(), Some(28) | Some(112))
}

impl From<LeaseError> for PromoteError {
    fn from(err: LeaseError) -> Self {
        Self::Lease(err)
    }
}

impl From<FenceError> for DurableError {
    fn from(_: FenceError) -> Self {
        Self::Fenced
    }
}

impl From<FenceError> for PromoteError {
    fn from(err: FenceError) -> Self {
        Self::Other(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_errors_on_newer_stored_tuple() {
        let err =
            decide_wal_publish(Some((2, 10, "h")), 1, 9, "h", PublishMode::Fresh).unwrap_err();
        assert_eq!(err, OffsetPublishError::FenceRegression);
    }

    #[test]
    fn republish_skips_newer_stored_tuple() {
        assert_eq!(
            decide_wal_publish(Some((2, 10, "h")), 1, 9, "h", PublishMode::Republish).unwrap(),
            PublishAction::AlreadyApplied
        );
    }

    #[test]
    fn same_tuple_same_hash_is_idempotent() {
        assert_eq!(
            decide_wal_publish(Some((1, 4, "abc")), 1, 4, "abc", PublishMode::Fresh).unwrap(),
            PublishAction::AlreadyApplied
        );
    }

    #[test]
    fn same_tuple_different_hash_is_corrupt() {
        assert!(matches!(
            decide_wal_publish(Some((1, 4, "abc")), 1, 4, "xyz", PublishMode::Fresh),
            Err(OffsetPublishError::Corrupt(_))
        ));
    }

    #[test]
    fn durable_error_display_clips_huge_protocol_detail() {
        let huge = "x".repeat(200_000);
        let err = DurableError::ProtocolMismatch(huge);
        let rendered = err.to_string();
        assert!(
            rendered.len() <= DURABLE_ERROR_DISPLAY_CEILING + 64,
            "error display was {} bytes",
            rendered.len()
        );
        assert!(rendered.contains("truncated"));
        assert!(rendered.contains("200000"));
    }
}
