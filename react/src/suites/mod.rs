use async_trait::async_trait;

use crate::flow_frame::FlowFrame;
use crate::providers::{
    CatalogProvider,
    DbtProvider,
    DatasetCatalogProvider,
    DynLlmProvider,
    EnvSecretsProvider,
    Keyspace,
    QueryProvider,
    RequestScope,
    SecretsProvider,
    StateStore,
    VectorStore,
};
use crate::adapters::storage::{InMemoryStorageAdapter, StorageAdapter};
use std::sync::Arc;

pub mod registry;
pub mod preflight;

pub mod shared;

pub mod skippr_ask_suite;
pub mod skippr_model_suite;

/// Context passed to suites.
///
/// This is the sole way suites access shared capabilities. The core ReAct runtime
/// should not reach into Skippr global configuration directly.
#[derive(Clone)]
pub struct SuiteCtx {
    pub storage: Arc<dyn StorageAdapter>,
    pub scope: RequestScope,
    pub keyspace: Arc<dyn Keyspace>,
    pub secrets: Arc<dyn SecretsProvider>,
    pub llm: DynLlmProvider,

    pub query: Option<Arc<dyn QueryProvider>>,
    pub datasets: Option<Arc<dyn DatasetCatalogProvider>>,
    pub catalog: Option<Arc<dyn CatalogProvider>>,
    pub vector: Option<Arc<dyn VectorStore>>,
    pub dbt: Option<Arc<dyn DbtProvider>>,
    pub state: Option<Arc<dyn StateStore>>,
}

impl SuiteCtx {
    pub fn new(
        storage: Arc<dyn StorageAdapter>,
        secrets: Arc<dyn SecretsProvider>,
        llm: DynLlmProvider,
        scope: RequestScope,
        keyspace: Arc<dyn Keyspace>,
    ) -> Self {
        Self { storage, scope, keyspace, secrets, llm, query: None, datasets: None, catalog: None, vector: None, dbt: None, state: None }
    }
}

impl Default for SuiteCtx {
    fn default() -> Self {
        Self {
            storage: Arc::new(InMemoryStorageAdapter::default()),
            scope: RequestScope { tenant: "default".to_string(), workspace: "default".to_string(), project_id: "default".to_string() },
            // Avoid env/config reads in default; callers should inject a real Keyspace.
            keyspace: Arc::new(crate::providers::DefaultKeyspace::new("unset".to_string())),
            secrets: Arc::new(EnvSecretsProvider::default()),
            // No implicit config/env reads in generic defaults; callers should inject a real LLM.
            llm: Arc::new(crate::llm::NullModel::new()),
            query: None,
            datasets: None,
            catalog: None,
            vector: None,
            dbt: None,
            state: None,
        }
    }
}

#[async_trait]
pub trait Suite: Send + Sync {
    fn id(&self) -> &'static str;

    async fn handle_new(
        &self,
        thread_id: &str,
        question: &str,
        agent_type: &str,
        ctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String>;

    async fn handle_open(
        &self,
        thread_id: &str,
        question: &str,
        agent_type: &str,
        ctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String>;

    async fn handle_user(
        &self,
        thread_id: &str,
        text: &str,
        agent_type: &str,
        ctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String>;
}

pub type DynSuite = Arc<dyn Suite>;

