use async_trait::async_trait;
use std::collections::HashSet;

use react_core::helpers::progress::ProgressUi;
use react_core::scope::RequestScope;

use super::dataset_catalog::DatasetCatalogProvider;

pub use super::catalog_types::*;

#[derive(Clone, Debug, Default)]
pub struct CatalogEnrichmentReport {
    pub dataset_total: usize,
    pub dataset_enriched_ok: usize,
    pub dataset_enriched_failed: usize,
    pub global_context_written: bool,
    pub global_context_error: Option<String>,
}

#[async_trait]
pub trait CatalogProvider: Send + Sync {
    async fn read_catalog(
        &self,
        scope: &RequestScope,
        dataset_id: &str,
    ) -> Result<Option<DataCatalog>, String>;

    async fn write_catalog(
        &self,
        scope: &RequestScope,
        dataset_id: &str,
        catalog: &DataCatalog,
    ) -> Result<(), String>;

    async fn infer_semantic(
        &self,
        scope: &RequestScope,
        dataset_id: &str,
    ) -> Result<SemanticModel, String>;

    async fn write_semantic(
        &self,
        scope: &RequestScope,
        dataset_id: &str,
        semantic: &SemanticModel,
    ) -> Result<(), String>;

    async fn read_semantic_profile(
        &self,
        scope: &RequestScope,
        dataset_id: &str,
    ) -> Result<Option<SemanticProfile>, String>;

    async fn write_semantic_profile(
        &self,
        scope: &RequestScope,
        dataset_id: &str,
        profile: &SemanticProfile,
    ) -> Result<(), String>;

    async fn build_all_with_progress(
        &self,
        scope: &RequestScope,
        query: &dyn DatasetCatalogProvider,
        dataset_ids: &HashSet<String>,
        progress: Option<&ProgressUi>,
    ) -> Result<(), String>;

    async fn run_llm_enrichment_all(
        &self,
        scope: &RequestScope,
        dataset_ids: &HashSet<String>,
    ) -> Result<CatalogEnrichmentReport, String>;
}
