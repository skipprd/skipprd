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

#[async_trait]
pub trait CatalogProvider: Send + Sync {
    async fn read_catalog(
        &self,
        scope: &crate::react::providers::RequestScope,
        namespace: &str,
    ) -> Result<Option<DataCatalog>, String>;
    async fn write_catalog(
        &self,
        scope: &crate::react::providers::RequestScope,
        namespace: &str,
        catalog: &DataCatalog,
    ) -> Result<(), String>;

    async fn infer_semantic(
        &self,
        scope: &crate::react::providers::RequestScope,
        namespace: &str,
    ) -> Result<SemanticModel, String>;
    async fn write_semantic(
        &self,
        scope: &crate::react::providers::RequestScope,
        namespace: &str,
        semantic: &SemanticModel,
    ) -> Result<(), String>;

    async fn build_all_with_progress(
        &self,
        scope: &crate::react::providers::RequestScope,
        query: &dyn crate::react::providers::DatasetCatalogProvider,
        namespaces: &HashMap<String, crate::discover::Metadata>,
        progress: Option<&crate::helpers::progress::ProgressUi>,
    ) -> Result<(), String>;

    async fn run_llm_enrichment_all(
        &self,
        scope: &crate::react::providers::RequestScope,
        namespaces: &HashMap<String, crate::discover::Metadata>,
    ) -> Result<(), String>;
}

/// Skippr's catalog provider implementation (current behavior).
///
/// This is intentionally Skippr-opinionated (sqlrt registry keys, S3 JSON backing, etc.)
/// while keeping `react` core generic by accessing it via the `CatalogProvider` trait.
#[derive(Clone)]
pub struct SkipprCatalogProvider {
    pub storage: Arc<dyn crate::adapters::storage::StorageAdapter>,
    pub keyspace: Arc<dyn crate::react::providers::Keyspace>,
    pub llm: Arc<dyn crate::llm::LargeLanguageModel>,
    pub llm_timeout_secs: u64,
    pub llm_batch_size: usize,
}

impl SkipprCatalogProvider {
    pub fn new(
        storage: Arc<dyn crate::adapters::storage::StorageAdapter>,
        keyspace: Arc<dyn crate::react::providers::Keyspace>,
        llm: Arc<dyn crate::llm::LargeLanguageModel>,
        llm_timeout_secs: u64,
        llm_batch_size: usize,
    ) -> Self {
        Self { storage, keyspace, llm, llm_timeout_secs, llm_batch_size }
    }
}

#[async_trait]
impl CatalogProvider for SkipprCatalogProvider {
    async fn read_catalog(
        &self,
        scope: &crate::react::providers::RequestScope,
        namespace: &str,
    ) -> Result<Option<DataCatalog>, String> {
        let key = self.keyspace.catalog_key(scope, namespace);
        match self.storage.get_json(&key).await {
            Ok(val) => Ok(Some(serde_json::from_value::<DataCatalog>(val).map_err(|e| e.to_string())?)),
            Err(_) => Ok(None),
        }
    }

    async fn write_catalog(
        &self,
        scope: &crate::react::providers::RequestScope,
        namespace: &str,
        catalog: &DataCatalog,
    ) -> Result<(), String> {
        // Match legacy behavior: YAML -> serde_yaml::Value -> JSON written.
        let key = self.keyspace.catalog_key(scope, namespace);
        let yaml = serde_yaml::to_string(catalog).map_err(|e| e.to_string())?;
        let value = serde_yaml::from_str::<serde_yaml::Value>(&yaml).unwrap_or(serde_yaml::Value::Null);
        let json_equiv = serde_json::to_value(value).unwrap_or(serde_json::Value::Null);
        self.storage.put_json(&key, &json_equiv).await?;
        Ok(())
    }

    async fn infer_semantic(
        &self,
        scope: &crate::react::providers::RequestScope,
        namespace: &str,
    ) -> Result<SemanticModel, String> {
        Ok(infer::infer_semantic_model_async(self.storage.clone(), self.keyspace.clone(), scope, namespace).await)
    }

    async fn write_semantic(
        &self,
        scope: &crate::react::providers::RequestScope,
        namespace: &str,
        semantic: &SemanticModel,
    ) -> Result<(), String> {
        semantic::write_semantic(self.storage.clone(), self.keyspace.clone(), scope, namespace, semantic).await
    }

    async fn build_all_with_progress(
        &self,
        scope: &crate::react::providers::RequestScope,
        query: &dyn crate::react::providers::DatasetCatalogProvider,
        namespaces: &HashMap<String, crate::discover::Metadata>,
        progress: Option<&crate::helpers::progress::ProgressUi>,
    ) -> Result<(), String> {
        let items = orchestrator::Orchestrator::build_all_with_progress(query, namespaces, progress).await?;
        for (dataset_id, cat) in items {
            self.write_catalog(scope, &dataset_id, &cat).await?;
        }
        Ok(())
    }

    async fn run_llm_enrichment_all(
        &self,
        scope: &crate::react::providers::RequestScope,
        namespaces: &HashMap<String, crate::discover::Metadata>,
    ) -> Result<(), String> {
        enrich::run_llm_enrichment_all(
            self.storage.clone(),
            self.keyspace.clone(),
            self.llm.clone(),
            scope,
            namespaces,
            self.llm_timeout_secs,
            self.llm_batch_size,
        ).await;
        Ok(())
    }
}

