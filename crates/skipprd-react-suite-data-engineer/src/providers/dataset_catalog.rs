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

    fn max_concurrency(&self) -> usize {
        super::DEFAULT_MAX_CONCURRENCY
    }
}

#[cfg(test)]
mod tests {
    use super::DatasetId;

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
}
