use async_trait::async_trait;
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct QueryResult {
    pub header: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub meta: Option<Value>,
}

// NOTE(item-61): Error strings from providers should follow the format
// `"{provider}: {detail}: {source_err}"` (e.g. `"athena: query failed: timeout"`).
// Existing providers are partially consistent; new code should follow this pattern.
#[async_trait]
pub trait QueryProvider: Send + Sync {
    async fn query(&self, sql: &str) -> Result<QueryResult, String>;
    async fn schema(&self, dataset_fqn: &str) -> Result<Vec<(String, String)>, String>;
    async fn sample(&self, dataset_fqn: &str, limit: usize) -> Result<Vec<Vec<String>>, String>;

    fn max_concurrency(&self) -> usize {
        super::DEFAULT_MAX_CONCURRENCY
    }
}
