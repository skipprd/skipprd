use async_trait::async_trait;
use serde_json::Value;

/// Minimal query result for generic suites.
///
/// This is intentionally JSON-friendly so suites can render it without depending
/// on DataFusion/Arrow types directly.
#[derive(Clone, Debug)]
pub struct QueryResult {
    pub header: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub meta: Option<Value>,
}

#[async_trait]
pub trait QueryProvider: Send + Sync {
    async fn query(&self, sql: &str) -> Result<QueryResult, String>;
    async fn schema(&self, dataset_fqn: &str) -> Result<Vec<(String, String)>, String>;
    async fn sample(&self, dataset_fqn: &str, limit: usize) -> Result<Vec<Vec<String>>, String>;

    /// Max in-flight queries the provider is configured to allow.
    ///
    /// Suites should treat this as the canonical concurrency limit for query batching.
    fn max_concurrency(&self) -> usize {
        crate::providers::limits::DEFAULT_ATHENA_MAX_CONCURRENCY
    }
}

