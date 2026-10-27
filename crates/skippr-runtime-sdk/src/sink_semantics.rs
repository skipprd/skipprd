use serde_derive::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum SinkRetrySemantics {
    TransactionalIdempotent,
    DeterministicOverwrite,
    FinalStateIdempotent,
    AtLeastOnce,
    #[default]
    NonRetryable,
}

impl SinkRetrySemantics {
    pub const fn equals(self, other: Self) -> bool {
        match (self, other) {
            (Self::TransactionalIdempotent, Self::TransactionalIdempotent) => true,
            (Self::DeterministicOverwrite, Self::DeterministicOverwrite) => true,
            (Self::FinalStateIdempotent, Self::FinalStateIdempotent) => true,
            (Self::AtLeastOnce, Self::AtLeastOnce) => true,
            (Self::NonRetryable, Self::NonRetryable) => true,
            _ => false,
        }
    }

    pub const fn requires_idempotent_replay(self) -> bool {
        match self {
            Self::TransactionalIdempotent
            | Self::DeterministicOverwrite
            | Self::FinalStateIdempotent => true,
            Self::AtLeastOnce | Self::NonRetryable => false,
        }
    }

    /// Host grouped compaction maps these retries to `SinkWriteSemantics::ExactOnce`.
    pub const fn grouped_writes_are_exact_once(self) -> bool {
        match self {
            Self::TransactionalIdempotent | Self::FinalStateIdempotent => true,
            Self::DeterministicOverwrite | Self::AtLeastOnce | Self::NonRetryable => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum SinkGroupingSupport {
    #[default]
    None,
    AppendOnlyBatches,
    CdcEncodedBatches,
    FinalStateBatches,
}

impl SinkGroupingSupport {
    pub const fn is_none(self) -> bool {
        match self {
            Self::None => true,
            Self::AppendOnlyBatches | Self::CdcEncodedBatches | Self::FinalStateBatches => false,
        }
    }

    pub const fn equals(self, other: Self) -> bool {
        match (self, other) {
            (Self::None, Self::None) => true,
            (Self::AppendOnlyBatches, Self::AppendOnlyBatches) => true,
            (Self::CdcEncodedBatches, Self::CdcEncodedBatches) => true,
            (Self::FinalStateBatches, Self::FinalStateBatches) => true,
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum SinkWriteSemantics {
    ExactOnce,
    IdempotentAtLeastOnce,
    #[default]
    AtLeastOnce,
}
