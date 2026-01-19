use async_trait::async_trait;
use std::sync::Arc;

use react_core::discover::Metadata;
use react_core::helpers::progress::ProgressUi;
use react_core::llm::{DynLlm, NullModel};
use react_core::providers::{
    CatalogProvider,
    DatasetCatalogProvider,
    SecretsProvider,
    StateStore,
    VectorStore,
    DbtProvider,
    QueryProvider,
    NullSecretsProvider,
};
use react_core::scope::RequestScope;
use react_core::keyspace::{DefaultKeyspace, Keyspace};
use react_core::storage::{InMemoryStorageAdapter, StorageAdapter};

use tokio::sync::mpsc::UnboundedSender;

use crate::flow_frame::FlowFrame;
use crate::config::ReactResolvedConfig;

/// Context passed to suites.
///
/// This is the sole way suites access shared capabilities. The core ReAct runtime
/// should not reach into any global configuration directly.
#[derive(Clone)]
pub struct SuiteCtx {
    pub storage: Arc<dyn StorageAdapter>,
    pub scope: RequestScope,
    pub keyspace: Arc<dyn Keyspace>,
    pub secrets: Arc<dyn SecretsProvider>,
    pub llm: DynLlm,
    /// Optional resolved runtime config (parsed by the runtime crate).
    pub resolved_config: Option<Arc<ReactResolvedConfig>>,
    /// Optional trace channel for streaming internal progress/debug lines (WS server may consume).
    pub trace_tx: Option<UnboundedSender<String>>,

    pub query: Option<Arc<dyn QueryProvider>>,
    pub datasets: Option<Arc<dyn DatasetCatalogProvider>>,
    pub catalog: Option<Arc<dyn CatalogProvider>>,
    pub vector: Option<Arc<dyn VectorStore>>,
    pub dbt: Option<Arc<dyn DbtProvider>>,
    pub state: Option<Arc<dyn StateStore>>,

    /// Back-compat stash for catalog build metadata/progress (kept here so suites can pass through).
    pub _metadata_stub: Option<Arc<Metadata>>,
    pub _progress_ui_stub: Option<Arc<ProgressUi>>,
}

impl SuiteCtx {
    pub fn new(
        storage: Arc<dyn StorageAdapter>,
        secrets: Arc<dyn SecretsProvider>,
        llm: DynLlm,
        scope: RequestScope,
        keyspace: Arc<dyn Keyspace>,
    ) -> Self {
        Self {
            storage,
            scope,
            keyspace,
            secrets,
            llm,
            resolved_config: None,
            trace_tx: None,
            query: None,
            datasets: None,
            catalog: None,
            vector: None,
            dbt: None,
            state: None,
            _metadata_stub: None,
            _progress_ui_stub: None,
        }
    }
}

impl Default for SuiteCtx {
    fn default() -> Self {
        Self {
            storage: Arc::new(InMemoryStorageAdapter::default()),
            scope: RequestScope { tenant: "default".to_string(), workspace: "default".to_string(), project_id: "default".to_string() },
            keyspace: Arc::new(DefaultKeyspace::new("unset".to_string())),
            secrets: Arc::new(NullSecretsProvider::default()),
            llm: Arc::new(NullModel::new()),
            resolved_config: None,
            trace_tx: None,
            query: None,
            datasets: None,
            catalog: None,
            vector: None,
            dbt: None,
            state: None,
            _metadata_stub: None,
            _progress_ui_stub: None,
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

