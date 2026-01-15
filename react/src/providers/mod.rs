//! Generic provider traits for suites.
//!
//! These are intentionally minimal and opinionated at a *capability* level:
//! storage, secrets, LLM, and optional query/catalog/vector/state providers.

use std::sync::Arc;

pub mod catalog;
pub mod athena_query;
pub mod dbt;
pub mod dataset_catalog_provider;
pub mod keyspace;
pub mod query;
pub mod scope;
pub mod secrets;
pub mod state;
pub mod vector;

pub use catalog::CatalogProvider;
pub use athena_query::{AthenaQueryProvider, AthenaSettings};
pub use dbt::{DbtProvider, DbtProjectProvider, DbtValidateArgs, DbtValidateResult};
pub use dataset_catalog_provider::{DatasetCatalogProvider, DatasetId};
pub use keyspace::{DefaultKeyspace, Keyspace};
pub use query::{QueryProvider, QueryResult};
pub use scope::RequestScope;
pub use secrets::{EnvSecretsProvider, SecretsProvider};
pub use state::StateStore;
pub use vector::{LanceVectorStore, VectorStore};

/// LLM provider is currently identical to the existing `skippr::llm::LargeLanguageModel`.
/// Suites should depend on this alias so we can evolve the underlying implementation
/// without changing suite code.
pub type LlmProvider = dyn crate::llm::LargeLanguageModel;

pub type DynLlmProvider = Arc<LlmProvider>;

