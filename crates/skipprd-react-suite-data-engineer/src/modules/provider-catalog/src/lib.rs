use async_trait::async_trait;
use std::collections::HashSet;
use std::sync::Arc;

pub mod builder;
pub mod enrich;
pub mod infer;
pub mod orchestrator;
pub mod semantic;
pub mod stats_from_catalog;
pub mod stats_repair;
pub mod type_parse;
pub mod types;
pub mod utils;

pub use types::{DataCatalog, SemanticModel, SemanticProfile};

use react_core::keyspace::encode_key_component;
use react_core::keyspace::Keyspace;
use react_core::llm::LargeLanguageModel;
use react_core::scope::RequestScope;
use react_core::storage::StorageAdapter;
use react_suite_data_engineer::providers::{CatalogProvider, DatasetId};

/// Default catalog provider implementation.
///
/// Opinionated about storage layout (S3 JSON backing, etc.) while keeping
/// react-core generic by accessing it via the `CatalogProvider` trait.
#[derive(Clone)]
pub struct DefaultCatalogProvider {
    pub storage: Arc<dyn StorageAdapter>,
    pub keyspace: Arc<dyn Keyspace>,
    pub llm: Arc<dyn LargeLanguageModel>,
    pub llm_timeout_secs: u64,
    pub llm_batch_size: usize,
    thread_id_fn: Option<Arc<dyn Fn() -> Option<String> + Send + Sync>>,
}

impl DefaultCatalogProvider {
    pub fn new(
        storage: Arc<dyn StorageAdapter>,
        keyspace: Arc<dyn Keyspace>,
        llm: Arc<dyn LargeLanguageModel>,
        llm_timeout_secs: u64,
        llm_batch_size: usize,
    ) -> Self {
        Self {
            storage,
            keyspace,
            llm,
            llm_timeout_secs,
            llm_batch_size,
            thread_id_fn: None,
        }
    }

    pub fn with_thread_id_fn(mut self, f: Arc<dyn Fn() -> Option<String> + Send + Sync>) -> Self {
        self.thread_id_fn = Some(f);
        self
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
        scope: &RequestScope,
        dataset_id: &str,
    ) -> Result<Option<DataCatalog>, String> {
        let canonical = Self::canonical_dataset_id(dataset_id)?;
        let key = self.keyspace.scoped_key(
            scope,
            &[
                "catalog",
                &format!("{}.yaml", encode_key_component(&canonical)),
            ],
        );
        match self.storage.get_json(&key).await {
            Ok(val) => Ok(Some(
                serde_json::from_value::<DataCatalog>(val).map_err(|e| e.to_string())?,
            )),
            Err(_) => Ok(None),
        }
    }

    async fn write_catalog(
        &self,
        scope: &RequestScope,
        dataset_id: &str,
        catalog: &DataCatalog,
    ) -> Result<(), String> {
        let canonical = Self::canonical_dataset_id(dataset_id)?;
        let key = self.keyspace.scoped_key(
            scope,
            &[
                "catalog",
                &format!("{}.yaml", encode_key_component(&canonical)),
            ],
        );
        let json_equiv = utils::yaml_to_json_value(catalog)?;
        self.storage
            .put_json(&key, &json_equiv)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn infer_semantic(
        &self,
        scope: &RequestScope,
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
        scope: &RequestScope,
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

    async fn read_semantic_profile(
        &self,
        scope: &RequestScope,
        dataset_id: &str,
    ) -> Result<Option<SemanticProfile>, String> {
        let canonical =
            if dataset_id == react_suite_data_engineer::providers::GLOBAL_SEMANTIC_DATASET_ID {
                dataset_id.to_string()
            } else {
                Self::canonical_dataset_id(dataset_id)?
            };
        semantic::read_semantic_profile(
            self.storage.clone(),
            self.keyspace.clone(),
            scope,
            &canonical,
        )
        .await
    }

    async fn write_semantic_profile(
        &self,
        scope: &RequestScope,
        dataset_id: &str,
        profile: &SemanticProfile,
    ) -> Result<(), String> {
        let canonical =
            if dataset_id == react_suite_data_engineer::providers::GLOBAL_SEMANTIC_DATASET_ID {
                dataset_id.to_string()
            } else {
                Self::canonical_dataset_id(dataset_id)?
            };
        semantic::write_semantic_profile(
            self.storage.clone(),
            self.keyspace.clone(),
            scope,
            &canonical,
            profile,
        )
        .await
    }

    async fn build_all_with_progress(
        &self,
        scope: &RequestScope,
        query: &dyn react_suite_data_engineer::providers::DatasetCatalogProvider,
        dataset_ids: &HashSet<String>,
        progress: Option<&react_core::helpers::progress::ProgressUi>,
    ) -> Result<(), String> {
        let thread = self.thread_id_fn.as_ref().and_then(|f| f()).map(|tid| {
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
        scope: &RequestScope,
        dataset_ids: &HashSet<String>,
    ) -> Result<react_suite_data_engineer::providers::CatalogEnrichmentReport, String> {
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
                tracing::warn!(
                    "global semantic context enrichment failed after dataset enrichment; continuing with catalog metadata repair path: {}",
                    e
                );
                Ok(report)
            }
        }
    }
}
