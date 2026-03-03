use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;

use crate::keyspace::{DefaultKeyspace, Keyspace};
use crate::llm::{DynLlm, NullModel};
use crate::providers::{
    CatalogProvider, DatasetCatalogProvider, DbtProvider, NullSecretsProvider,
    NullWarehouseProvider, QueryProvider, SecretsProvider, StateStore, VectorStore,
    WarehouseProvider,
};
use crate::resolved_config::ReactResolvedConfig;
use crate::scope::RequestScope;
use crate::storage::{InMemoryStorageAdapter, StorageAdapter};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

/// Standardized suite output type.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum FlowFrame {
    Final {
        kind: String,
        payload: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        display: Option<String>,
    },
    Review {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        meta: Option<Value>,
    },
    AwaitUser { prompt: String },
    AwaitApproval { prompt: String },
}

/// Context passed to suites.
#[derive(Clone)]
pub struct SuiteCtx {
    pub storage: Arc<dyn StorageAdapter>,
    pub scope: RequestScope,
    pub keyspace: Arc<dyn Keyspace>,
    pub secrets: Arc<dyn SecretsProvider>,
    pub llm: DynLlm,
    pub resolved_config: Option<Arc<ReactResolvedConfig>>,
    pub trace_tx: Option<UnboundedSender<String>>,

    pub query: Option<Arc<dyn QueryProvider>>,
    pub datasets: Option<Arc<dyn DatasetCatalogProvider>>,
    pub warehouse: Arc<dyn WarehouseProvider>,
    pub catalog: Option<Arc<dyn CatalogProvider>>,
    pub vector: Option<Arc<dyn VectorStore>>,
    pub dbt: Option<Arc<dyn DbtProvider>>,
    pub state: Option<Arc<dyn StateStore>>,
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
            warehouse: Arc::new(NullWarehouseProvider::default()),
            catalog: None,
            vector: None,
            dbt: None,
            state: None,
        }
    }
}

impl Default for SuiteCtx {
    fn default() -> Self {
        Self {
            storage: Arc::new(InMemoryStorageAdapter::default()),
            scope: RequestScope {
                tenant: "default".to_string(),
                workspace: "default".to_string(),
                project_id: "default".to_string(),
            },
            keyspace: Arc::new(DefaultKeyspace::new("unset".to_string())),
            secrets: Arc::new(NullSecretsProvider::default()),
            llm: Arc::new(NullModel::new()),
            resolved_config: None,
            trace_tx: None,
            query: None,
            datasets: None,
            warehouse: Arc::new(NullWarehouseProvider::default()),
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

    fn label(&self) -> &'static str {
        self.id()
    }

    fn supported_agent_types(&self) -> Vec<String> {
        vec!["ask".to_string()]
    }

    fn default_agent_type(&self) -> &'static str {
        "ask"
    }

    fn phase_order(&self, _agent_type: &str) -> Vec<String> {
        Vec::new()
    }

    async fn load_ws_plans(
        &self,
        _thread_id: &str,
        _ctx: &SuiteCtx,
    ) -> Result<Vec<serde_json::Value>, String> {
        Ok(Vec::new())
    }

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

/// Compile-time workflow contract for suites using the core workflow kernel.
///
/// This keeps core generic while giving suites a typed place to declare:
/// - phase/reason enums
/// - state/reducer model
/// - transition/backtrack semantics
pub trait WorkflowSuiteContract {
    type Phase: Copy + Eq + Send + Sync + 'static;
    type ReasonCode: Copy + Eq + Send + Sync + 'static;
    type State: Clone + Send + Sync + 'static;
    type Event: Send + Sync + 'static;

    fn phase_as_str(phase: Self::Phase) -> &'static str;
    fn reason_as_str(reason: Self::ReasonCode) -> &'static str;
    fn is_backtrack(from: Self::Phase, to: Self::Phase) -> bool;
    fn replan_backtrack_cap() -> usize;
    fn pre_turn(_state: &Self::State) -> crate::workflow::PreTurnDirective {
        crate::workflow::PreTurnDirective::Proceed
    }

    fn reduce(state: &mut Self::State, event: Self::Event);
}

/// Typed node contract for suites that use an explicit workflow state-machine node.
///
/// This lets runtime orchestration derive phase execution from one canonical node source,
/// rather than recomputing control flow from multiple independent state fields.
pub trait WorkflowNodeContract: WorkflowSuiteContract {
    type Node: Copy + Eq + Send + Sync + 'static;

    fn node_from_state(state: &Self::State) -> Self::Node;
    fn phase_from_node(node: Self::Node) -> Self::Phase;
}

pub type DynSuite = Arc<dyn Suite>;

pub struct SuiteRegistry {
    suites: HashMap<&'static str, DynSuite>,
}

impl SuiteRegistry {
    pub fn new() -> Self {
        Self {
            suites: HashMap::new(),
        }
    }

    pub fn register<S: Suite + 'static>(&mut self, suite: S) {
        let id = suite.id();
        self.suites.insert(id, Arc::new(suite));
    }

    pub fn get(&self, suite_id: &str) -> Option<DynSuite> {
        self.suites.get(suite_id).cloned()
    }

    pub fn list_ids(&self) -> Vec<&'static str> {
        let mut out: Vec<&'static str> = self.suites.keys().copied().collect();
        out.sort();
        out
    }
}
