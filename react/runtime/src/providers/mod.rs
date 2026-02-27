//! Provider traits and runtime implementations.
//!
//! Trait definitions live in react_core::providers (via re-export).
//! Concrete implementations (catalog orchestrator, keyspace, secrets, etc.) live here.

pub mod catalog;
pub mod dataset_catalog_provider;
pub mod keyspace;
pub mod scope;
pub mod secrets;
pub mod state;
pub mod type_parse;
pub mod vector;

pub use react_core::providers::{
    CatalogProvider, DatasetCatalogProvider, DatasetId, DbtProvider, DbtValidateArgs,
    DbtValidateResult, NullSecretsProvider, NullWarehouseProvider, QueryProvider, QueryResult,
    SecretsProvider, StateStore, VectorStore, WarehouseProvider,
    DEFAULT_WAREHOUSE_MAX_CONCURRENCY,
};
pub use react_core::scope::RequestScope;

pub use keyspace::{DefaultKeyspace, Keyspace, LocalKeyspace};
pub use secrets::EnvSecretsProvider;
pub use type_parse::FlattenedField;
pub use vector::LanceVectorStore;
