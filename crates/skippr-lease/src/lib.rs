//! Clock-free pipeline lease domain types.
//!
//! Production storage is DynamoDB (`skippr-lease-store-dynamodb`) or Cloud
//! tables (`skippr-lease-store-cloud-tables`). Tests use [`MemoryLeaseStore`].
//! Single-node `disk`/`s3` modes do not use a lease store.

mod clock;
mod error;
mod guard;
mod identity;
mod membership;
mod paths;
mod protocol;
mod store;

pub use clock::{
    race_deadline, AsyncSleeper, Clock, MonoInstant, Sleeper, SystemClock, TestClock, TestSleeper,
    TokioSleeper,
};
pub use error::{
    is_enospc, DurableError, FenceError, LeaseError, OffsetPublishError, PathError, PromoteError,
    ReplicaNack, ShutdownError,
};
pub use guard::{
    LeaseGuard, PipelineLifecycle, PipelineRole, WriteAuthority, LEASE_RENEW_PERIOD, LEASE_TIMEOUT,
    REPLICATION_FACTOR, WRITE_QUORUM,
};
pub use identity::{
    ClusterId, CommitIndex, HostId, LeaseEpoch, LeaseObservation, LeaseSession, NodeId,
    PipelineKey, SegmentId,
};
pub use membership::{ClusterMembershipStore, MembershipRecord, NodeAd};
pub use paths::{encode_path_component, validate_path_component, PipelinePaths};
pub use protocol::{acquire_pipeline, renew_pipeline, renew_until_lost, to_session};
pub use store::{MembershipAd, MemoryLeaseStore, MemoryMembershipStore, PipelineLeaseStore};

pub const PROTOCOL_MIN: u32 = 2;
pub const PROTOCOL_MAX: u32 = 2;
pub const CURRENT_PROTOCOL: u32 = 2;
pub const GENESIS_HASH: [u8; 32] = [0u8; 32];
pub const CONTROL_FRAME_MAX_BYTES: usize = 16 * 1024 * 1024;
pub const PAYLOAD_CHUNK_BYTES: usize = 1024 * 1024;
/// Max mutation envelopes in one FetchEntries control frame. Payloads stream after the frame.
pub const FETCH_ENTRIES_MAX_COUNT: usize = 8;
/// Replica placement treats a volume as under pressure below this free-byte floor.
pub const DISK_PRESSURE_FREE_BYTES: u64 = 256 * 1024 * 1024;
pub const RPC_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
pub const RPC_DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);
