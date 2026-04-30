use async_trait::async_trait;
use react_core::provider_traits::{ScoredVectorRecord, StoredVectorRecord, VectorStore};
use react_core::resolved_config::{S3Credentials, S3CredentialsProvider};
use react_core::scope::RequestScope;
use react_module_provider_vector_lance::lance_store::{Chunk, LanceDbStore};
use std::sync::Arc;

#[derive(Clone)]
pub enum LanceStorageOptions {
    Static(Vec<(String, String)>),
    Refreshable(Arc<dyn S3CredentialsProvider>),
}

impl Default for LanceStorageOptions {
    fn default() -> Self {
        Self::Static(Vec::new())
    }
}

pub fn lance_storage_options_from_credentials(
    credentials: &S3Credentials,
) -> Vec<(String, String)> {
    let mut options = vec![
        (
            "aws_access_key_id".into(),
            credentials.access_key_id.clone(),
        ),
        (
            "aws_secret_access_key".into(),
            credentials.secret_access_key.clone(),
        ),
        ("aws_region".into(), credentials.region.clone()),
    ];
    if let Some(token) = credentials.session_token.as_ref() {
        options.push(("aws_session_token".into(), token.clone()));
    }
    options
}

/// Skippr's default LanceDB-backed vector store.
#[derive(Clone)]
pub struct LanceVectorStore {
    pub uri_prefix: String,
    storage_options: LanceStorageOptions,
}

impl LanceVectorStore {
    pub fn new(uri_prefix: String) -> Self {
        Self {
            uri_prefix,
            storage_options: LanceStorageOptions::default(),
        }
    }

    pub fn with_storage_options(mut self, opts: LanceStorageOptions) -> Self {
        self.storage_options = opts;
        self
    }

    async fn storage_options(&self) -> Result<Vec<(String, String)>, String> {
        match &self.storage_options {
            LanceStorageOptions::Static(options) => Ok(options.clone()),
            LanceStorageOptions::Refreshable(provider) => provider
                .s3_credentials()
                .await
                .map(|credentials| lance_storage_options_from_credentials(&credentials)),
        }
    }

    async fn store_for(&self, scope: &RequestScope) -> Result<LanceDbStore, String> {
        let uri = format!(
            "{}/{}/{}/{}/lancedb",
            self.uri_prefix, scope.tenant, scope.workspace, scope.project_id
        );
        Ok(LanceDbStore::new(&uri).with_storage_options(self.storage_options().await?))
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
        self.store_for(scope).await?.upsert(&mapped).await
    }

    async fn query(
        &self,
        scope: &RequestScope,
        query_vec: &[f32],
        k: usize,
        namespace: Option<&str>,
    ) -> Result<Vec<ScoredVectorRecord>, String> {
        let out = self
            .store_for(scope)
            .await?
            .query(query_vec, k, namespace)
            .await?;
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
            .await?
            .delete_thread_embeddings(thread_id)
            .await
    }

    async fn delete_project_embeddings(&self, scope: &RequestScope) -> Result<(), String> {
        self.store_for(scope)
            .await?
            .delete_pipeline_embeddings()
            .await
    }

    async fn delete_namespace(&self, scope: &RequestScope, namespace: &str) -> Result<(), String> {
        self.store_for(scope)
            .await?
            .delete_namespace(namespace)
            .await
    }

    async fn delete_ids_with_prefix(
        &self,
        scope: &RequestScope,
        prefix: &str,
    ) -> Result<(), String> {
        self.store_for(scope)
            .await?
            .delete_ids_with_prefix(prefix)
            .await
    }
}
