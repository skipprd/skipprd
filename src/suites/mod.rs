use std::sync::Arc;

use async_trait::async_trait;

use crate::flows::adapter::FlowFrame;

pub mod registry;
pub mod skippr_ask_suite;
pub mod skippr_model_suite;
pub mod preflight;

/// Context passed to suites. This will be extended to include core adapters
/// (env/config, storage, etc.) once those interfaces are introduced.
#[derive(Clone, Default)]
pub struct SuiteCtx {}

#[async_trait]
pub trait Suite: Send + Sync {
    fn id(&self) -> &'static str;

    /// Handle a new thread/session initialization.
    ///
    /// `agent_type` is explicitly provided by the client and must be validated by the suite.
    async fn handle_new(
        &self,
        thread_id: &str,
        question: &str,
        agent_type: &str,
        ctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String>;

    /// Handle opening/resuming a thread.
    async fn handle_open(
        &self,
        thread_id: &str,
        question: &str,
        agent_type: &str,
        ctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String>;

    /// Handle a user message within an existing thread.
    async fn handle_user(
        &self,
        thread_id: &str,
        text: &str,
        agent_type: &str,
        ctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String>;
}

pub type DynSuite = Arc<dyn Suite>;

