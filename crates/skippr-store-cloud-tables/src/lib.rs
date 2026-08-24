//! Cloud tables backends for clustered skipprd (D37 mesh JWT).

mod catalog;
mod lease;
mod membership;
mod offset;
mod pipeline;

pub use catalog::{file_io_for_warehouse, CloudTablesCatalog};
pub use lease::CloudTablesLeaseStore;
pub use membership::CloudTablesMembershipStore;
pub use offset::CloudTablesOffsetStore;
pub use pipeline::{
    CloudTablesPipelineRegistry, MemoryPipelineRegistry, PipelineKind, PipelineRecord,
    PipelineRegistry, OTEL_LOGS, OTEL_METRICS, OTEL_TRACES, PIPELINES_TABLE, PLATFORM_TENANT,
    PLATFORM_WORKSPACE,
};
pub use skippr_tables_client::{parse_tables_endpoint, TablesClient, TablesClientError};
