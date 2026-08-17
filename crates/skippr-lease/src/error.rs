use crate::identity::{CommitIndex, NodeId};
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
    #[error("diverged: {0}")]
    Diverged(String),
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

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum DurableError {
    #[error("fenced")]
    Fenced,
    #[error("fenced after commit")]
    FencedAfterCommit,
    #[error("quorum lost: {0}")]
    QuorumLost(String),
    #[error("quorum lost: prepared index {0} has no committed peer copy")]
    UnprovenPrepared(u64),
    #[error("diverged: {0}")]
    Diverged(String),
    #[error("corrupt offset: {0}")]
    CorruptOffset(String),
    #[error("io: {0}")]
    Io(String),
    #[error("protocol mismatch: {0}")]
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

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum OffsetPublishError {
    #[error("corrupt offset payload: {0}")]
    Corrupt(String),
    #[error("transient offset publish error: {0}")]
    Transient(String),
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum PromoteError {
    #[error("lease error: {0}")]
    Lease(LeaseError),
    #[error("no hash-consistent head is reachable")]
    NoHeadAvailable,
    #[error("diverged at index {index:?}: {detail}")]
    Diverged { index: CommitIndex, detail: String },
    #[error("required schema missing: {0}")]
    SchemaMissing(String),
    #[error("replica assignment failed: {0}")]
    Replica(String),
    #[error("prepared index {index} has no committed peer copy")]
    UnprovenPrepared { index: u64 },
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
    #[error("diverged")]
    Diverged,
    #[error("disk full")]
    DiskFull,
    #[error("payload hash mismatch")]
    PayloadHashMismatch,
    #[error("unknown pipeline")]
    UnknownPipeline,
    #[error("invalid primary endpoint")]
    InvalidPrimaryEndpoint,
    #[error("no snapshot")]
    NoSnapshot,
    #[error("empty snapshot")]
    EmptySnapshot,
    #[error("snapshot hash mismatch")]
    SnapshotHashMismatch,
    #[error("snapshot unavailable")]
    SnapshotUnavailable,
}

impl ReplicaNack {
    pub fn into_durable(self) -> DurableError {
        match self {
            Self::StaleEpoch => DurableError::StaleEpoch,
            Self::NotAssigned | Self::NotCaughtUp => DurableError::NotCaughtUp,
            Self::Diverged => DurableError::Diverged(self.to_string()),
            Self::DiskFull => DurableError::DiskFull,
            Self::PayloadHashMismatch | Self::UnknownPipeline | Self::InvalidPrimaryEndpoint => {
                DurableError::ProtocolMismatch(self.to_string())
            }
            Self::NoSnapshot
            | Self::EmptySnapshot
            | Self::SnapshotHashMismatch
            | Self::SnapshotUnavailable => DurableError::Io(self.to_string()),
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
            Self::Io(err.to_string())
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
