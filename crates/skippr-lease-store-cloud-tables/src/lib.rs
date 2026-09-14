//! Cloud Tables lease, membership, and pipeline registry stores.

mod lease;
mod membership;
mod pipeline;

pub use lease::CloudTablesLeaseStore;
pub use membership::CloudTablesMembershipStore;
pub use pipeline::{
    CloudTablesPipelineRegistry, MemoryPipelineRegistry, PipelineKind, PipelineRecord,
    PipelineRegistry, OTEL_LOGS, OTEL_METRICS, OTEL_TRACES, PIPELINES_TABLE, PLATFORM_TENANT,
    PLATFORM_WORKSPACE,
};
