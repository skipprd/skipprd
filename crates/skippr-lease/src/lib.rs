//! Clock-free pipeline lease domain types.
//!
//! Production storage is DynamoDB (`skippr-lease-store-dynamodb`). Tests use
//! [`MemoryLeaseStore`]. Disk and S3 ingest acquire a writer lease through the
//! same [`PipelineLeaseStore`] (sled locally; DynamoDB when that is the offset
//! backend).

mod clock;
mod conformance;
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
pub use conformance::assert_pipeline_lease_conformance;
pub use error::{
    decide_wal_publish, is_enospc, DurableError, FenceError, LeaseError, OffsetPublishError,
    PathError, PromoteError, PublishAction, PublishMode, ReplicaNack, ShutdownError,
};
pub use guard::{
    LeaseGuard, PipelineLifecycle, PipelineRole, WriteAuthority, LEASE_RENEW_PERIOD, LEASE_TIMEOUT,
    REPLICATION_FACTOR, WRITE_QUORUM,
};
pub use identity::{
    ClusterId, HostId, LeaseEpoch, LeaseObservation, LeaseSession, NodeId, PipelineKey, SegmentId,
};
pub use membership::{ClusterMembershipStore, MembershipRecord, NodeAd};
pub use paths::{encode_path_component, validate_path_component, PipelinePaths};
pub use protocol::{acquire_pipeline, renew_pipeline, renew_until_lost, to_session};
pub use store::{MembershipAd, MemoryLeaseStore, MemoryMembershipStore, PipelineLeaseStore};

pub const PROTOCOL_MIN: u32 = 3;
pub const PROTOCOL_MAX: u32 = 3;
pub const CURRENT_PROTOCOL: u32 = 3;
/// SK of the pipeline lease item in DynamoDB (same table as catalog).
pub const LEASE_SK: &str = "lease";
pub const LEASE_ATTR_OWNER: &str = "owner_node";
pub const LEASE_ATTR_EPOCH: &str = "epoch";
pub const LEASE_ATTR_RELEASED: &str = "released";
pub const CONTROL_FRAME_MAX_BYTES: usize = 16 * 1024 * 1024;
pub const PAYLOAD_CHUNK_BYTES: usize = 1024 * 1024;
/// Replica placement treats a volume as under pressure below this free-byte floor.
pub const DISK_PRESSURE_FREE_BYTES: u64 = 256 * 1024 * 1024;
pub const RPC_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
pub const RPC_DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);
