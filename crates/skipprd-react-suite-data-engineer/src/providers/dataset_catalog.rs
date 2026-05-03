use async_trait::async_trait;

use super::catalog_types::DatasetStats;
use super::stats::DatasetFieldStats;

/// Provider-facing dataset identifier in `<catalog>.<database>.<table>` form.
///
/// This type lives on the provider trait boundary. The internal equivalent is
/// [`crate::references::DatasetRef`], which uses `schema` for
/// the middle segment. `From` impls exist in both directions.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DatasetId {
    pub catalog: String,
    pub database: String,
    pub table: String,
}

impl DatasetId {
    pub fn fqn(&self) -> String {
        format!("{}.{}.{}", self.catalog, self.database, self.table)
    }

    pub fn parse_fqn_strict(raw: &str) -> Result<Self, String> {
        let parts: Vec<&str> = raw.trim().split('.').collect();
        if parts.len() != 3 {
            return Err(
                "dataset_id must be fully-qualified <catalog>.<database>.<table>".to_string(),
            );
        }
        let catalog = parts[0].trim();
        let database = parts[1].trim();
        let table = parts[2].trim();
        if catalog.is_empty() || database.is_empty() || table.is_empty() {
            return Err(
                "dataset_id must be fully-qualified <catalog>.<database>.<table>".to_string(),
            );
        }
        Ok(Self {
            catalog: catalog.to_string(),
            database: database.to_string(),
            table: table.to_string(),
        })
    }
}

#[async_trait]
pub trait DatasetCatalogProvider: Send + Sync {
    async fn list_datasets(&self) -> Result<Vec<DatasetId>, String>;
    async fn get_dataset_schema(
        &self,
        dataset: &DatasetId,
    ) -> Result<Vec<(String, String)>, String>;

    async fn get_dataset_stats(
        &self,
        dataset: &DatasetId,
        max_fields: usize,
    ) -> Result<(DatasetFieldStats, DatasetStats), String>;

    fn evidence_capabilities(&self) -> ProviderEvidenceCapabilities;

    fn max_concurrency(&self) -> usize {
        super::DEFAULT_MAX_CONCURRENCY
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EvidenceCapability {
    Supported,
    BestEffort { caveat: String },
    Unsupported { reason: String },
}

impl EvidenceCapability {
    pub fn supported() -> Self {
        Self::Supported
    }

    pub fn best_effort(caveat: impl Into<String>) -> Self {
        Self::BestEffort {
            caveat: caveat.into(),
        }
    }

    pub fn unsupported(reason: impl Into<String>) -> Self {
        Self::Unsupported {
            reason: reason.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderEvidenceCapabilities {
    pub schema_columns: EvidenceCapability,
    pub exact_distinct: EvidenceCapability,
    pub composite_distinct: EvidenceCapability,
    pub numeric_parse_probe: EvidenceCapability,
    pub timestamp_detection: EvidenceCapability,
    pub relationship_constraints: EvidenceCapability,
    pub row_count_probe: EvidenceCapability,
    pub row_preservation_probe: EvidenceCapability,
}

impl ProviderEvidenceCapabilities {
    pub fn sql_warehouse_without_relationships() -> Self {
        Self {
            schema_columns: EvidenceCapability::supported(),
            exact_distinct: EvidenceCapability::supported(),
            composite_distinct: EvidenceCapability::best_effort(
                "requires provider-specific COUNT DISTINCT tuple SQL",
            ),
            numeric_parse_probe: EvidenceCapability::supported(),
            timestamp_detection: EvidenceCapability::supported(),
            relationship_constraints: EvidenceCapability::unsupported(
                "constraint metadata is not wired into deterministic evidence yet",
            ),
            row_count_probe: EvidenceCapability::supported(),
            row_preservation_probe: EvidenceCapability::best_effort(
                "requires query-specific row-count comparison",
            ),
        }
    }

    pub fn schema_only(reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self {
            schema_columns: EvidenceCapability::supported(),
            exact_distinct: EvidenceCapability::unsupported(reason.clone()),
            composite_distinct: EvidenceCapability::unsupported(reason.clone()),
            numeric_parse_probe: EvidenceCapability::unsupported(reason.clone()),
            timestamp_detection: EvidenceCapability::best_effort(
                "native column types can identify temporal fields when schema is available",
            ),
            relationship_constraints: EvidenceCapability::unsupported(reason.clone()),
            row_count_probe: EvidenceCapability::unsupported(reason.clone()),
            row_preservation_probe: EvidenceCapability::unsupported(reason),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DatasetId, EvidenceCapability, ProviderEvidenceCapabilities};

    #[test]
    fn parse_fqn_strict_accepts_canonical_triplet() {
        let ds = DatasetId::parse_fqn_strict("AwsDataCatalog.test_raw.raw_customers").unwrap();
        assert_eq!(ds.catalog, "AwsDataCatalog");
        assert_eq!(ds.database, "test_raw");
        assert_eq!(ds.table, "raw_customers");
    }

    #[test]
    fn parse_fqn_strict_rejects_non_triplet_inputs() {
        assert!(DatasetId::parse_fqn_strict("raw_customers").is_err());
        assert!(DatasetId::parse_fqn_strict("a.b").is_err());
        assert!(DatasetId::parse_fqn_strict("a..c").is_err());
    }

    #[test]
    fn evidence_capabilities_make_unsupported_explicit() {
        let caps = ProviderEvidenceCapabilities::schema_only("stats unavailable");
        assert!(matches!(
            caps.exact_distinct,
            EvidenceCapability::Unsupported { .. }
        ));
    }
}
