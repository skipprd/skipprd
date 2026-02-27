use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::scope::RequestScope;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VectorChunk {
    pub id: String,
    pub kind: String, // dataset|field|doc
    pub dataset_id: String,
    pub field: Option<String>,
    pub text: String,
    pub vector: Vec<f32>,
    pub meta: Value,
    pub epoch: u64,
}

#[derive(Clone, Debug)]
pub struct ScoredVectorChunk {
    pub item: VectorChunk,
    pub score: f32,
}

/// Vector store is optional but often shared across multiple suite tools.
#[async_trait]
pub trait VectorStore: Send + Sync {
    async fn upsert(&self, scope: &RequestScope, items: &[VectorChunk]) -> Result<(), String>;
    async fn query(
        &self,
        scope: &RequestScope,
        query_vec: &[f32],
        k: usize,
        scope_filter: Option<&str>,
    ) -> Result<Vec<ScoredVectorChunk>, String>;
    async fn delete_thread_embeddings(
        &self,
        scope: &RequestScope,
        thread_id: &str,
    ) -> Result<(), String>;
    async fn delete_project_embeddings(&self, scope: &RequestScope) -> Result<(), String>;
}
