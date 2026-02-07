//! Generic provider traits for suites.
//!
//! These are intentionally minimal and opinionated at a *capability* level:
//! storage, secrets, LLM, and optional query/catalog/vector/state providers.

pub mod athena_query;
pub mod catalog;
pub mod dataset_catalog_provider;
pub mod dbt;
pub mod keyspace;
pub mod postgres;
pub mod scope;
pub mod secrets;
pub mod state;
pub mod type_parse;
pub mod vector;

pub use react_core::providers::{
    CatalogProvider, DatasetCatalogProvider, DatasetId, DbtProvider, DbtValidateArgs,
    DbtValidateResult, QueryProvider, QueryResult, SecretsProvider, StateStore, VectorStore,
};
pub use react_core::scope::RequestScope;

pub use athena_query::{AthenaQueryProvider, AthenaSettings};
pub use dbt::DbtProjectProvider;
pub use keyspace::{DefaultKeyspace, Keyspace, LocalKeyspace}; // runtime impl of core Keyspace
pub use postgres::{PostgresProvider, PostgresSettings};
pub use secrets::EnvSecretsProvider;
pub use type_parse::FlattenedField;
pub use vector::LanceVectorStore;

// NOTE: LLM provider types moved to `react_core::llm`.
