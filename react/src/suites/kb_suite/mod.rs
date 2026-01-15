use async_trait::async_trait;

use crate::agent::{Agent, AgentCtx, DefaultPolicy, RunOutcome};
use crate::flow_frame::FlowFrame;
use crate::session::ThreadStore;
use crate::suites::{Suite, SuiteCtx};
use crate::tools::ToolRegistry;
use std::sync::Arc;

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
        // Optional: allow users to inspect stored artifacts via existing shared tool.
        registry.register(crate::suites::shared::tools::artifacts::ArtifactsTool);

        // Vector provider is required for kb suite.
        if sctx.vector.is_none() {
            return Err("vector provider missing".to_string());
        }
        Ok(registry)
    }

    async fn run_kb(thread_id: &str, question: &str, sctx: &SuiteCtx) -> Result<Vec<FlowFrame>, String> {
        let sys = prompts::system_prompt();
        let tools_card = prompts::tool_card();
        let registry = Self::build_tools(sctx)?;
        let thread_store = ThreadStore::new(sctx.storage.clone(), sctx.scope.clone(), sctx.keyspace.clone());

        let actx = AgentCtx {
            top_k: 20,
            per_step_timeout_secs: 30,
            max_steps: 30,
            thread_id: Some(thread_id.to_string()),
            progress_tx: None,
            pre_step_tx: None,
            agent_name: Some("kb".to_string()),
            policy: Arc::new(DefaultPolicy),
            llm: sctx.llm.clone(),
            storage: sctx.storage.clone(),
            scope: sctx.scope.clone(),
            keyspace: sctx.keyspace.clone(),
            dbt: None,
            vector: sctx.vector.clone(),
            thread_store: Some(thread_store),
            dataset_candidates: Vec::new(),
        };

        match Agent::run_until_block(&registry, &actx, sys, tools_card, question).await {
            Ok(RunOutcome::Final { thread_id: _tid, result }) => Ok(vec![FlowFrame::Final {
                answer: result.answer,
                sql: result.sql,
            }]),
            Ok(RunOutcome::AwaitUser { thread_id: _tid, prompt }) => Ok(vec![FlowFrame::AwaitUser { prompt }]),
            Ok(RunOutcome::AwaitApproval { thread_id: _tid, prompt }) => Ok(vec![FlowFrame::AwaitUser { prompt }]),
            Err(e) => Err(e),
        }
    }
}

#[async_trait]
impl Suite for KbSuite {
    fn id(&self) -> &'static str {
        "kb"
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

