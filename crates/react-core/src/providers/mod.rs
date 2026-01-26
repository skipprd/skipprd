//! Capability-level provider traits used by suites.
//!
//! Implementations live in the runtime crate.

pub mod secrets;
pub mod state;
pub mod dataset_catalog_provider;
pub mod catalog;
pub mod query;
pub mod vector;
pub mod dbt;
pub mod limits;

pub use secrets::{SecretsProvider, NullSecretsProvider};
pub use state::StateStore;
pub use dataset_catalog_provider::{DatasetCatalogProvider, DatasetId};
pub use catalog::{CatalogProvider, DataCatalog, SemanticModel};
pub use query::{QueryProvider, QueryResult};
pub use vector::{VectorStore, VectorChunk, ScoredVectorChunk};
pub use dbt::{DbtProvider, DbtValidateArgs, DbtValidateResult};
pub use limits::{ATHENA_MAX_CONCURRENCY_CAP, DEFAULT_ATHENA_MAX_CONCURRENCY, clamp_athena_concurrency};

