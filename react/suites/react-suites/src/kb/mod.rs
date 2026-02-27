use async_trait::async_trait;
use std::sync::Arc;

use react_core::agent::{Agent, AgentCtx, DefaultPolicy, RunOutcome};
use react_core::session::ThreadStore;
use react_core::tools::ToolRegistry;

use crate::flow_frame::FlowFrame;
use crate::suite::{Suite, SuiteCtx};

pub struct KbSuite;

pub mod prompts;
pub mod tools;

impl KbSuite {
    fn validate_agent_type(agent_type: &str) -> Result<(), String> {
        if agent_type != "kb" {
            return Err(format!(
                "invalid agent_type '{}' for suite 'kb' (expected 'kb')",
                agent_type
            ));
        }
        Ok(())
    }

    fn build_tools(sctx: &SuiteCtx) -> Result<ToolRegistry, String> {
        let mut registry = ToolRegistry::new();
        registry.register(tools::kb_ingest_dir::KbIngestDirTool);
        registry.register(tools::kb_search::KbSearchTool);

        // Vector provider is required for kb suite.
        if sctx.vector.is_none() {
            return Err("vector provider missing".to_string());
        }
        Ok(registry)
    }

    async fn run_kb(
        thread_id: &str,
        question: &str,
        sctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String> {
        let sys = prompts::system_prompt();
        let tools_card = prompts::tool_card();
        let registry = Self::build_tools(sctx)?;
        let thread_store = ThreadStore::new(
            sctx.storage.clone(),
            sctx.scope.clone(),
            sctx.keyspace.clone(),
        );

        let actx = AgentCtx {
            top_k: 20,
            per_step_timeout_secs: 30,
            max_steps: 30,
            thread_id: Some(thread_id.to_string()),
            progress_tx: None,
            pre_step_tx: None,
            trace_tx: sctx.trace_tx.clone(),
            agent_name: Some("kb".to_string()),
            policy: Arc::new(DefaultPolicy),
            llm: sctx.llm.clone(),
            storage: sctx.storage.clone(),
            scope: sctx.scope.clone(),
            keyspace: sctx.keyspace.clone(),
            query: None,
            warehouse: sctx.warehouse.clone(),
            dbt: None,
            vector: sctx.vector.clone(),
            thread_store: Some(thread_store),
            exec_ctx: None,
            runtime: None,
        };

        match Agent::run_until_block(&registry, &actx, sys, tools_card, question, {
            // OpenAI Responses output_tokens includes reasoning tokens; ensure we have enough
            // room for the JSON payload by defaulting to LOW reasoning and a higher token cap.
            let max_out: u32 = std::env::var("LLM_KB_MAX_TOKENS")
                .ok()
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(4_000)
                .max(800)
                .min(32_000);
            let effort = match std::env::var("LLM_KB_REASONING_EFFORT")
                .ok()
                .map(|s| s.trim().to_lowercase())
                .as_deref()
            {
                Some("none") => react_core::llm::ReasoningEffort::None,
                Some("low") | None | Some("") => react_core::llm::ReasoningEffort::Low,
                Some("medium") => react_core::llm::ReasoningEffort::Medium,
                Some("high") => react_core::llm::ReasoningEffort::High,
                _ => react_core::llm::ReasoningEffort::Low,
            };
            react_core::llm::LlmCallOptions {
                prompt_id: "kb.run",
                thread_id: Some(thread_id.to_string()),
                expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
                max_output_tokens: Some(max_out),
                reasoning_effort: Some(effort),
                temperature: None,
                top_p: None,
            }
        })
        .await
        {
            Ok(RunOutcome::Final {
                thread_id: _tid,
                result,
            }) => Ok(vec![FlowFrame::Final {
                kind: result.kind.into(),
                payload: result.payload,
                display: result.display,
            }]),
            Ok(RunOutcome::AwaitUser {
                thread_id: _tid,
                prompt,
            }) => Ok(vec![FlowFrame::AwaitUser { prompt }]),
            Ok(RunOutcome::AwaitApproval {
                thread_id: _tid,
                prompt,
            }) => Ok(vec![FlowFrame::AwaitApproval { prompt }]),
            Err(e) => Err(e),
        }
    }
}

#[async_trait]
impl Suite for KbSuite {
    fn id(&self) -> &'static str {
        "kb"
    }

    fn label(&self) -> &'static str {
        "KB"
    }

    fn supported_agent_types(&self) -> Vec<String> {
        vec!["kb".to_string()]
    }

    fn default_agent_type(&self) -> &'static str {
        "kb"
    }

    fn phase_order(&self, _agent_type: &str) -> Vec<String> {
        // kb suite does not expose internal phases (single-pass).
        Vec::new()
    }

    async fn handle_new(
        &self,
        thread_id: &str,
        question: &str,
        agent_type: &str,
        ctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String> {
        Self::validate_agent_type(agent_type)?;
        Self::run_kb(thread_id, question, ctx).await
    }

    async fn handle_open(
        &self,
        thread_id: &str,
        question: &str,
        agent_type: &str,
        ctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String> {
        Self::validate_agent_type(agent_type)?;
        Self::run_kb(thread_id, question, ctx).await
    }

    async fn handle_user(
        &self,
        thread_id: &str,
        text: &str,
        agent_type: &str,
        ctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String> {
        Self::validate_agent_type(agent_type)?;
        Self::run_kb(thread_id, text, ctx).await
    }
}
