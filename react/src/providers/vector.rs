use async_trait::async_trait;
use std::sync::Arc;

/// Vector store is optional but often shared across multiple suite tools.
///
/// For now, we reuse the existing `react::vector::lance_store` chunk types.
pub type VectorChunk = crate::vector::lance_store::Chunk;
pub type ScoredVectorChunk = crate::vector::lance_store::ScoredChunk;

#[async_trait]
pub trait VectorStore: Send + Sync {
    async fn upsert(&self, scope: &crate::providers::RequestScope, items: &[VectorChunk]) -> Result<(), String>;
    async fn query(&self, scope: &crate::providers::RequestScope, query_vec: &[f32], k: usize, scope_filter: Option<&str>) -> Result<Vec<ScoredVectorChunk>, String>;
    async fn delete_thread_embeddings(&self, scope: &crate::providers::RequestScope, thread_id: &str) -> Result<(), String>;
    async fn delete_project_embeddings(&self, scope: &crate::providers::RequestScope) -> Result<(), String>;
}

/// Skippr's vector store implementation (LanceDB-on-S3).
#[derive(Clone)]
pub struct SkipprLanceVectorStore {
    pub keyspace: Arc<dyn crate::providers::Keyspace>,
    pub scope: crate::providers::RequestScope,
}

impl SkipprLanceVectorStore {
    pub fn new(keyspace: Arc<dyn crate::providers::Keyspace>, scope: crate::providers::RequestScope) -> Self {
        Self { keyspace, scope }
    }

    fn store_for(&self, scope: &crate::providers::RequestScope) -> crate::vector::lance_store::LanceDbStore {
        let uri = self.keyspace.lancedb_uri(scope);
        crate::vector::lance_store::LanceDbStore::new(&uri)
    }

    pub fn global_dbt_examples_store(&self) -> crate::vector::global_lance_store::GlobalLanceDbStore {
        let uri = self.keyspace.global_dbt_examples_lancedb_uri();
        crate::vector::global_lance_store::GlobalLanceDbStore::new(uri)
    }
}

#[async_trait]
impl VectorStore for SkipprLanceVectorStore {
    async fn upsert(&self, scope: &crate::providers::RequestScope, items: &[VectorChunk]) -> Result<(), String> {
        self.store_for(scope).upsert(items).await
    }

    async fn query(&self, scope: &crate::providers::RequestScope, query_vec: &[f32], k: usize, scope_filter: Option<&str>) -> Result<Vec<ScoredVectorChunk>, String> {
        self.store_for(scope).query(query_vec, k, scope_filter).await
    }

    async fn delete_thread_embeddings(&self, scope: &crate::providers::RequestScope, thread_id: &str) -> Result<(), String> {
        self.store_for(scope).delete_thread_embeddings(thread_id).await
    }

    async fn delete_project_embeddings(&self, scope: &crate::providers::RequestScope) -> Result<(), String> {
        self.store_for(scope).delete_pipeline_embeddings().await
    }
}

