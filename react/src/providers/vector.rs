use std::sync::Arc;

use async_trait::async_trait;
use react_core::providers::{ScoredVectorChunk, VectorChunk, VectorStore};
use react_core::scope::RequestScope;

/// Default vector store implementation (LanceDB-on-S3).
#[derive(Clone)]
pub struct LanceVectorStore {
    pub keyspace: Arc<dyn crate::providers::Keyspace>,
    pub scope: RequestScope,
}

impl LanceVectorStore {
    pub fn new(keyspace: Arc<dyn crate::providers::Keyspace>, scope: RequestScope) -> Self {
        Self { keyspace, scope }
    }

    fn store_for(&self, scope: &RequestScope) -> crate::vector::lance_store::LanceDbStore {
        let uri = self.keyspace.lancedb_uri(scope);
        crate::vector::lance_store::LanceDbStore::new(&uri)
    }

    pub fn global_dbt_examples_store(&self) -> crate::vector::global_lance_store::GlobalLanceDbStore {
        let uri = self.keyspace.global_dbt_examples_lancedb_uri();
        crate::vector::global_lance_store::GlobalLanceDbStore::new(uri)
    }
}

#[async_trait]
impl VectorStore for LanceVectorStore {
    async fn upsert(&self, scope: &RequestScope, items: &[VectorChunk]) -> Result<(), String> {
        let mapped: Vec<crate::vector::lance_store::Chunk> = items
            .iter()
            .cloned()
            .map(|c| crate::vector::lance_store::Chunk {
                id: c.id,
                kind: c.kind,
                dataset_id: c.dataset_id,
                field: c.field,
                text: c.text,
                vector: c.vector,
                meta: c.meta,
                epoch: c.epoch,
            })
            .collect();
        self.store_for(scope).upsert(&mapped).await
    }

    async fn query(
        &self,
        scope: &RequestScope,
        query_vec: &[f32],
        k: usize,
        scope_filter: Option<&str>,
    ) -> Result<Vec<ScoredVectorChunk>, String> {
        let out = self.store_for(scope).query(query_vec, k, scope_filter).await?;
        Ok(out
            .into_iter()
            .map(|s| ScoredVectorChunk {
                item: VectorChunk {
                    id: s.item.id,
                    kind: s.item.kind,
                    dataset_id: s.item.dataset_id,
                    field: s.item.field,
                    text: s.item.text,
                    vector: s.item.vector,
                    meta: s.item.meta,
                    epoch: s.item.epoch,
                },
                score: s.score,
            })
            .collect())
    }

    async fn delete_thread_embeddings(&self, scope: &RequestScope, thread_id: &str) -> Result<(), String> {
        self.store_for(scope).delete_thread_embeddings(thread_id).await
    }

    async fn delete_project_embeddings(&self, scope: &RequestScope) -> Result<(), String> {
        self.store_for(scope).delete_pipeline_embeddings().await
    }
}

