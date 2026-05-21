use async_trait::async_trait;

use super::catalog_types::DatasetStats;
use super::dataset_catalog::{DatasetCatalogProvider, DatasetId, ProviderEvidenceCapabilities};
use super::query::{QueryProvider, QueryResult};
use super::stats::DatasetFieldStats;

// TODO(item-67): All provider traits return `Result<..., String>`. Replace with a
// typed error enum (e.g. `ProviderError { kind, message, source }`) to enable
// programmatic error handling and avoid string matching on error messages.
pub trait WarehouseNaming: Send + Sync {
    fn kind(&self) -> crate::de_config::WarehouseKind;

    fn parse_dataset_fqn(&self, dataset_fqn: &str) -> Result<DatasetId, String>;

    fn format_dataset_fqn(&self, id: &DatasetId) -> String {
        id.fqn()
    }

    fn quote_ident(&self, ident: &str) -> String;

    fn dbt_runtime_namespace_ident(&self, ident: &str) -> String {
        self.quote_ident(ident)
    }

    fn dbt_custom_schema_policy(&self) -> crate::providers::DbtCustomSchemaPolicy {
        crate::providers::DbtCustomSchemaPolicy::AdapterDefault
    }

    fn quote_fqn(&self, id: &DatasetId) -> String {
        format!(
            "{}.{}.{}",
            self.quote_ident(&id.catalog),
            self.quote_ident(&id.database),
            self.quote_ident(&id.table)
        )
    }

    fn format_dbt_model_relation_fqn(&self, id: &DatasetId) -> String {
        self.quote_fqn(id)
    }

    fn dbt_model_relation_lookup_id(&self, id: &DatasetId) -> DatasetId {
        id.clone()
    }

    fn sql_prompt_rules(&self) -> Vec<&'static str> {
        vec![]
    }

    fn sql_remediation_rules(&self) -> Vec<&'static str> {
        vec![]
    }
}

// NOTE(item-62): Provider constructors differ by necessity:
// - Athena: `async from_settings()` / `async from_env()` (AWS SDK init is async)
// - BigQuery: `async from_settings() -> Result` (GCP client can fail)
// - Postgres: `from_settings()` (sync; connection is deferred to first query)
// These differences reflect underlying SDK requirements, not an inconsistency.
pub trait WarehouseProvider: QueryProvider + DatasetCatalogProvider + WarehouseNaming {}
impl<T> WarehouseProvider for T where T: QueryProvider + DatasetCatalogProvider + WarehouseNaming {}

#[derive(Clone, Default)]
pub struct NullWarehouseProvider;

#[async_trait]
impl QueryProvider for NullWarehouseProvider {
    async fn query(&self, _sql: &str) -> Result<QueryResult, String> {
        Err("warehouse provider not configured".to_string())
    }
    async fn schema(&self, _dataset_fqn: &str) -> Result<Vec<(String, String)>, String> {
        Err("warehouse provider not configured".to_string())
    }
    async fn sample(&self, _dataset_fqn: &str, _limit: usize) -> Result<Vec<Vec<String>>, String> {
        Err("warehouse provider not configured".to_string())
    }
}

#[async_trait]
impl DatasetCatalogProvider for NullWarehouseProvider {
    async fn list_datasets(&self) -> Result<Vec<DatasetId>, String> {
        Err("warehouse provider not configured".to_string())
    }
    async fn get_dataset_schema(
        &self,
        _dataset: &DatasetId,
    ) -> Result<Vec<(String, String)>, String> {
        Err("warehouse provider not configured".to_string())
    }
    async fn get_dataset_stats(
        &self,
        _dataset: &DatasetId,
        _max_fields: usize,
    ) -> Result<(DatasetFieldStats, DatasetStats), String> {
        Err("warehouse provider not configured".to_string())
    }

    fn evidence_capabilities(&self) -> ProviderEvidenceCapabilities {
        ProviderEvidenceCapabilities::schema_only("warehouse provider not configured")
    }
}

impl WarehouseNaming for NullWarehouseProvider {
    fn kind(&self) -> crate::de_config::WarehouseKind {
        crate::de_config::WarehouseKind::default()
    }

    fn parse_dataset_fqn(&self, _dataset_fqn: &str) -> Result<DatasetId, String> {
        Err("warehouse provider not configured".to_string())
    }

    fn quote_ident(&self, ident: &str) -> String {
        format!("\"{}\"", ident.replace('"', "\"\""))
    }
}
