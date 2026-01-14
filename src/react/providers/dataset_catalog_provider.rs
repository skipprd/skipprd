use async_trait::async_trait;

/// Canonical dataset identifier for catalog building and tool UX.
///
/// For Athena/Glue, `catalog` will typically be `"AwsDataCatalog"` (or similar),
/// but we keep it explicit for future portability.
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
}

/// Provider capability for enumerating datasets and retrieving schema/stats, used by catalog build.
///
/// This intentionally keeps the catalog build job from calling any SDKs directly.
#[async_trait]
pub trait DatasetCatalogProvider: Send + Sync {
    async fn list_datasets(&self) -> Result<Vec<DatasetId>, String>;
    async fn get_dataset_schema(&self, dataset: &DatasetId) -> Result<Vec<(String, String)>, String>;

    /// Optional: Provider-computed stats for a dataset. Providers may return an error if unsupported.
    async fn get_dataset_stats(
        &self,
        dataset: &DatasetId,
        max_fields: usize,
    ) -> Result<(crate::discover::stats::NamespaceStats, crate::react::providers::catalog::types::DatasetStats), String>;
}

