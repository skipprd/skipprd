//! Capability-level provider traits used by suites.
//!
//! Implementations live in the runtime crate.

pub mod catalog;
pub mod dataset_catalog_provider;
pub mod dbt;
pub mod limits;
pub mod query;
pub mod secrets;
pub mod state;
pub mod vector;
pub mod warehouse;

pub use catalog::{CatalogProvider, DataCatalog, SemanticModel};
pub use dataset_catalog_provider::{DatasetCatalogProvider, DatasetId};
pub use dbt::{DbtProvider, DbtValidateArgs, DbtValidateResult};
pub use limits::DEFAULT_WAREHOUSE_MAX_CONCURRENCY;
pub use query::{QueryProvider, QueryResult};
pub use secrets::{NullSecretsProvider, SecretsProvider};
pub use state::StateStore;
pub use vector::{ScoredVectorChunk, VectorChunk, VectorStore};
pub use warehouse::{NullWarehouseProvider, WarehouseNaming, WarehouseProvider};
