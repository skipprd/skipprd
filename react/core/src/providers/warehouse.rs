use async_trait::async_trait;

use crate::providers::{DatasetCatalogProvider, DatasetId, QueryProvider, QueryResult};

/// Provider-agnostic warehouse interface: query + catalog + mechanical naming/quoting.
///
/// Implementations live in the runtime crate (`react`).
pub trait WarehouseNaming: Send + Sync {
    fn kind(&self) -> &'static str;

    /// Parse a dataset identifier.
    ///
    /// Providers may accept shorthand forms, but suites should prefer canonical 3-part FQNs.
    fn parse_dataset_fqn(&self, dataset_fqn: &str) -> Result<DatasetId, String>;

    fn format_dataset_fqn(&self, id: &DatasetId) -> String {
        id.fqn()
    }

    fn quote_ident(&self, ident: &str) -> String;

    fn quote_fqn(&self, id: &DatasetId) -> String {
        format!(
            "{}.{}.{}",
            self.quote_ident(&id.catalog),
            self.quote_ident(&id.database),
            self.quote_ident(&id.table)
        )
    }

    /// Provider-owned SQL authoring rules for LLM prompts.
    fn sql_prompt_rules(&self) -> Vec<&'static str> {
        vec![]
    }

    /// Provider-owned SQL remediation rules for repair prompts.
    fn sql_remediation_rules(&self) -> Vec<&'static str> {
        vec![]
    }

    /// Optional provider-owned deterministic SQL guardrail.
    /// Return a human-readable reason when SQL should be rejected.
    fn unsupported_sql_reason(&self, _sql: &str) -> Option<String> {
        None
    }
}

/// Full warehouse capability: query + dataset catalog + naming helpers.
pub trait WarehouseProvider: QueryProvider + DatasetCatalogProvider + WarehouseNaming {}
impl<T> WarehouseProvider for T where T: QueryProvider + DatasetCatalogProvider + WarehouseNaming {}

/// Default placeholder warehouse used for tests/defaults when the runtime does not wire providers.
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
    ) -> Result<
        (
            crate::discover::stats::DatasetFieldStats,
            crate::providers::catalog::types::DatasetStats,
        ),
        String,
    > {
        Err("warehouse provider not configured".to_string())
    }
}

impl WarehouseNaming for NullWarehouseProvider {
    fn kind(&self) -> &'static str {
        "none"
    }

    fn parse_dataset_fqn(&self, _dataset_fqn: &str) -> Result<DatasetId, String> {
        Err("warehouse provider not configured".to_string())
    }

    fn quote_ident(&self, ident: &str) -> String {
        format!("\"{}\"", ident.replace('"', "\"\""))
    }
}
