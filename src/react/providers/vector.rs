use async_trait::async_trait;
use std::sync::Arc;

/// Vector store is optional but often shared across multiple suite tools.
///
/// For now, we reuse the existing `react::vector::lance_store` chunk types.
pub type VectorChunk = crate::react::vector::lance_store::Chunk;
pub type ScoredVectorChunk = crate::react::vector::lance_store::ScoredChunk;

#[async_trait]
pub trait VectorStore: Send + Sync {
    async fn upsert(&self, pipeline: &str, items: &[VectorChunk]) -> Result<(), String>;
    async fn query(&self, pipeline: &str, query_vec: &[f32], k: usize, scope: Option<&str>) -> Result<Vec<ScoredVectorChunk>, String>;
    async fn delete_thread_embeddings(&self, pipeline: &str, thread_id: &str) -> Result<(), String>;
    async fn delete_pipeline_embeddings(&self, pipeline: &str) -> Result<(), String>;
}

/// Skippr's vector store implementation (LanceDB-on-S3).
#[derive(Clone)]
pub struct SkipprLanceVectorStore {
    pub keyspace: Arc<dyn crate::react::providers::Keyspace>,
    pub scope: crate::react::providers::RequestScope,
}

impl SkipprLanceVectorStore {
    pub fn new(keyspace: Arc<dyn crate::react::providers::Keyspace>, scope: crate::react::providers::RequestScope) -> Self {
        Self { keyspace, scope }
    }

    fn store_for(&self, pipeline: &str) -> crate::react::vector::lance_store::LanceDbStore {
        let uri = self.keyspace.lancedb_uri(&self.scope, pipeline);
        crate::react::vector::lance_store::LanceDbStore::new(&uri)
    }

    pub fn global_dbt_examples_store(&self) -> crate::react::vector::global_lance_store::GlobalLanceDbStore {
        let uri = self.keyspace.global_dbt_examples_lancedb_uri();
        crate::react::vector::global_lance_store::GlobalLanceDbStore::new(uri)
    }
}

#[async_trait]
impl VectorStore for SkipprLanceVectorStore {
    async fn upsert(&self, pipeline: &str, items: &[VectorChunk]) -> Result<(), String> {
        self.store_for(pipeline).upsert(items).await
    }

    async fn query(&self, pipeline: &str, query_vec: &[f32], k: usize, scope: Option<&str>) -> Result<Vec<ScoredVectorChunk>, String> {
        self.store_for(pipeline).query(query_vec, k, scope).await
    }

    async fn delete_thread_embeddings(&self, pipeline: &str, thread_id: &str) -> Result<(), String> {
        self.store_for(pipeline).delete_thread_embeddings(thread_id).await
    }

    async fn delete_pipeline_embeddings(&self, pipeline: &str) -> Result<(), String> {
        self.store_for(pipeline).delete_pipeline_embeddings().await
    }
}

