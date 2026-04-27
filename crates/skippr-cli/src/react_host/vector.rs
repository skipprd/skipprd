use async_trait::async_trait;
use react_core::provider_traits::{ScoredVectorRecord, StoredVectorRecord, VectorStore};
use react_core::scope::RequestScope;
use react_module_provider_vector_lance::lance_store::{Chunk, LanceDbStore};

/// Skippr's default LanceDB-backed vector store.
#[derive(Clone)]
pub struct LanceVectorStore {
    pub uri_prefix: String,
    storage_options: Vec<(String, String)>,
}

impl LanceVectorStore {
    pub fn new(uri_prefix: String) -> Self {
        Self {
            uri_prefix,
            storage_options: Vec::new(),
        }
    }

    pub fn with_storage_options(mut self, opts: Vec<(String, String)>) -> Self {
        self.storage_options = opts;
        self
    }

    fn store_for(&self, scope: &RequestScope) -> LanceDbStore {
        let uri = format!(
            "{}/{}/{}/{}/lancedb",
            self.uri_prefix, scope.tenant, scope.workspace, scope.project_id
        );
        LanceDbStore::new(&uri).with_storage_options(self.storage_options.clone())
    }
}

#[async_trait]
impl VectorStore for LanceVectorStore {
    async fn upsert(
        &self,
        scope: &RequestScope,
        items: &[StoredVectorRecord],
    ) -> Result<(), String> {
        let mapped: Vec<Chunk> = items
            .iter()
            .cloned()
            .map(|c| Chunk {
                id: c.id,
                namespace: c.namespace,
                text: c.text,
                vector: c.vector,
                metadata_json: c.metadata_json,
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
        namespace: Option<&str>,
    ) -> Result<Vec<ScoredVectorRecord>, String> {
        let out = self.store_for(scope).query(query_vec, k, namespace).await?;
        Ok(out
            .into_iter()
            .map(|s| ScoredVectorRecord {
                item: StoredVectorRecord {
                    id: s.item.id,
                    namespace: s.item.namespace,
                    text: s.item.text,
                    vector: s.item.vector,
                    metadata_json: s.item.metadata_json,
                    epoch: s.item.epoch,
                },
                score: s.score,
            })
            .collect())
    }

    async fn delete_thread_embeddings(
        &self,
        scope: &RequestScope,
        thread_id: &str,
    ) -> Result<(), String> {
        self.store_for(scope)
            .delete_thread_embeddings(thread_id)
            .await
    }

    async fn delete_project_embeddings(&self, scope: &RequestScope) -> Result<(), String> {
        self.store_for(scope).delete_pipeline_embeddings().await
    }

    async fn delete_namespace(&self, scope: &RequestScope, namespace: &str) -> Result<(), String> {
        self.store_for(scope).delete_namespace(namespace).await
    }

    async fn delete_ids_with_prefix(
        &self,
        scope: &RequestScope,
        prefix: &str,
    ) -> Result<(), String> {
        self.store_for(scope).delete_ids_with_prefix(prefix).await
    }
}
