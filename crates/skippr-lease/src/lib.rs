//! Pipeline identity, catalog epoch, paths, and durable error types.
//!
//! Writer leases, membership, and replica quorum are not part of this crate.

mod error;
mod identity;
mod paths;

pub use error::{
    decide_wal_publish, is_enospc, DurableError, OffsetPublishError, PathError, PublishAction,
    PublishMode,
};
pub use identity::{LeaseEpoch, NodeId, PipelineKey, SegmentId};
pub use paths::{encode_path_component, validate_path_component, PipelinePaths};

pub const CONTROL_FRAME_MAX_BYTES: usize = 16 * 1024 * 1024;
pub const PAYLOAD_CHUNK_BYTES: usize = 1024 * 1024;
pub const RPC_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
