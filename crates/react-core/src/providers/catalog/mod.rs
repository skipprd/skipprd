use async_trait::async_trait;
use std::collections::HashMap;

use crate::discover::Metadata;
use crate::helpers::progress::ProgressUi;
use crate::providers::dataset_catalog_provider::DatasetCatalogProvider;
use crate::scope::RequestScope;

pub mod types;

pub use types::{DataCatalog, SemanticModel};

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

    async fn build_all_with_progress(
        &self,
        scope: &RequestScope,
        query: &dyn DatasetCatalogProvider,
        dataset_ids: &HashMap<String, Metadata>,
        progress: Option<&ProgressUi>,
    ) -> Result<(), String>;

    async fn run_llm_enrichment_all(
        &self,
        scope: &RequestScope,
        dataset_ids: &HashMap<String, Metadata>,
    ) -> Result<(), String>;
}
