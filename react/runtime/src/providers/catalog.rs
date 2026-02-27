use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;

pub mod builder;
pub mod enrich;
pub mod infer;
pub mod orchestrator;
pub mod semantic;
pub mod stats_from_catalog;
pub mod types;
pub mod utils;

pub use types::{DataCatalog, SemanticModel};

use react_core::providers::{CatalogProvider, DatasetId};

/// Default catalog provider implementation (current behavior).
///
/// This is opinionated about storage layout (S3 JSON backing, etc.) while keeping `react` core
/// generic by accessing it via the `CatalogProvider` trait.
#[derive(Clone)]
pub struct DefaultCatalogProvider {
    pub storage: Arc<dyn crate::adapters::storage::StorageAdapter>,
    pub keyspace: Arc<dyn crate::providers::Keyspace>,
    pub llm: Arc<dyn crate::llm::LargeLanguageModel>,
    pub llm_timeout_secs: u64,
    pub llm_batch_size: usize,
}

impl DefaultCatalogProvider {
    pub fn new(
        storage: Arc<dyn crate::adapters::storage::StorageAdapter>,
        keyspace: Arc<dyn crate::providers::Keyspace>,
        llm: Arc<dyn crate::llm::LargeLanguageModel>,
        llm_timeout_secs: u64,
        llm_batch_size: usize,
    ) -> Self {
        Self {
            storage,
            keyspace,
            llm,
            llm_timeout_secs,
            llm_batch_size,
        }
    }

    fn canonical_dataset_id(dataset_id: &str) -> Result<String, String> {
        DatasetId::parse_fqn_strict(dataset_id)
            .map(|d| d.fqn())
            .map_err(|_| {
                format!(
                    "catalog dataset_id must be canonical <catalog>.<database>.<table>; got '{}'",
                    dataset_id
                )
            })
    }
}

#[async_trait]
impl CatalogProvider for DefaultCatalogProvider {
    async fn read_catalog(
        &self,
        scope: &crate::providers::RequestScope,
        dataset_id: &str,
    ) -> Result<Option<DataCatalog>, String> {
        let canonical = Self::canonical_dataset_id(dataset_id)?;
        let key = self.keyspace.catalog_key(scope, &canonical);
        match self.storage.get_json(&key).await {
            Ok(val) => Ok(Some(
                serde_json::from_value::<DataCatalog>(val).map_err(|e| e.to_string())?,
            )),
            Err(_) => Ok(None),
        }
    }

    async fn write_catalog(
        &self,
        scope: &crate::providers::RequestScope,
        dataset_id: &str,
        catalog: &DataCatalog,
    ) -> Result<(), String> {
        // Match legacy behavior: YAML -> serde_yaml::Value -> JSON written.
        let canonical = Self::canonical_dataset_id(dataset_id)?;
        let key = self.keyspace.catalog_key(scope, &canonical);
        let yaml = serde_yaml::to_string(catalog).map_err(|e| e.to_string())?;
        let value =
            serde_yaml::from_str::<serde_yaml::Value>(&yaml).unwrap_or(serde_yaml::Value::Null);
        let json_equiv = serde_json::to_value(value).unwrap_or(serde_json::Value::Null);
        self.storage.put_json(&key, &json_equiv).await?;
        Ok(())
    }

    async fn infer_semantic(
        &self,
        scope: &crate::providers::RequestScope,
        dataset_id: &str,
    ) -> Result<SemanticModel, String> {
        Ok(infer::infer_semantic_model_async(
            self.storage.clone(),
            self.keyspace.clone(),
            scope,
            dataset_id,
        )
        .await)
    }

    async fn write_semantic(
        &self,
        scope: &crate::providers::RequestScope,
        dataset_id: &str,
        semantic: &SemanticModel,
    ) -> Result<(), String> {
        semantic::write_semantic(
            self.storage.clone(),
            self.keyspace.clone(),
            scope,
            dataset_id,
            semantic,
        )
        .await
    }

    async fn build_all_with_progress(
        &self,
        scope: &crate::providers::RequestScope,
        query: &dyn crate::providers::dataset_catalog_provider::DatasetCatalogProvider,
        dataset_ids: &HashMap<String, crate::discover::Metadata>,
        progress: Option<&crate::helpers::progress::ProgressUi>,
    ) -> Result<(), String> {
        let thread = crate::llm::thread_ctx::current_thread_id().map(|tid| {
            let store = react_core::session::ThreadStore::new(
                self.storage.clone(),
                scope.clone(),
                self.keyspace.clone(),
            );
            (store, tid)
        });
        let items = orchestrator::Orchestrator::build_all_with_progress(
            query,
            dataset_ids,
            progress,
            thread,
        )
        .await?;
        for (dataset_id, cat) in items {
            self.write_catalog(scope, &dataset_id, &cat).await?;
        }
        Ok(())
    }

    async fn run_llm_enrichment_all(
        &self,
        scope: &crate::providers::RequestScope,
        dataset_ids: &HashMap<String, crate::discover::Metadata>,
    ) -> Result<react_core::providers::catalog::CatalogEnrichmentReport, String> {
        let mut report = enrich::run_llm_enrichment_all(
            self.storage.clone(),
            self.keyspace.clone(),
            self.llm.clone(),
            scope,
            dataset_ids,
            self.llm_timeout_secs,
            self.llm_batch_size,
        )
        .await?;
        // Global context pass: infer cross-dataset meaning + likely audiences.
        match enrich::run_llm_global_context_enrichment_all(
            self.storage.clone(),
            self.keyspace.clone(),
            self.llm.clone(),
            scope,
            dataset_ids,
            self.llm_timeout_secs,
        )
        .await
        {
            Ok(global_written) => {
                report.global_context_written = global_written;
                Ok(report)
            }
            Err(e) => {
                report.global_context_error = Some(e.clone());
                Err(format!(
                    "global semantic context enrichment failed after dataset enrichment: {e}"
                ))
            }
        }
    }
}
