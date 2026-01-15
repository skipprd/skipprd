use async_trait::async_trait;

use crate::flow_frame::FlowFrame;
use crate::agent::{Agent, AgentCtx, DatasetCandidate, RunOutcome, SqlValidatedFinalPolicy};
use crate::tools::ToolRegistry;
use crate::suites::preflight::PreflightProvider;
use crate::suites::{Suite, SuiteCtx};
use crate::session::ThreadStore;

pub struct SkipprAskSuite;

pub mod tools;
pub mod prompts;
pub mod flows;

impl SkipprAskSuite {
    fn validate_agent_type(agent_type: &str) -> Result<(), String> {
        if agent_type != "ask" {
            return Err(format!(
                "invalid agent_type '{}' for suite 'skippr_ask' (expected 'ask')",
                agent_type
            ));
        }
        Ok(())
    }

    fn build_tools(sctx: &SuiteCtx) -> Result<ToolRegistry, String> {
        use crate::suites::shared::tools::{
            artifacts::ArtifactsTool,
            sql_run::SqlRunTool,
            sql_sample::SqlSampleTool,
            sql_schema::SqlSchemaTool,
            sql_stats::SqlStatsTool,
            vect_query::VectQueryTool,
        };
        use crate::suites::skippr_ask_suite::tools::{
            ask_approval::AskApprovalTool,
            ask_user::AskUserTool,
        };

        let query = sctx
            .query
            .as_ref()
            .ok_or_else(|| "query provider missing".to_string())?
            .clone();

        let mut registry = ToolRegistry::new();
        registry.register(SqlRunTool { query: query.clone() });
        registry.register(SqlSchemaTool { query: query.clone(), datasets: sctx.datasets.clone(), catalog: sctx.catalog.clone() });
        registry.register(SqlStatsTool { catalog: sctx.catalog.clone(), datasets: sctx.datasets.clone() });
        registry.register(SqlSampleTool { query: query.clone() });
        registry.register(VectQueryTool);
        registry.register(AskUserTool);
        registry.register(AskApprovalTool);
        registry.register(ArtifactsTool);
        Ok(registry)
    }

    pub(crate) async fn run_ask(thread_id: &str, question: &str, sctx: &SuiteCtx) -> Result<Vec<FlowFrame>, String> {
        let sys = crate::util::time_context::with_time_context(crate::suites::skippr_ask_suite::prompts::system_prompt());
        let tools_card = crate::suites::skippr_ask_suite::prompts::tool_card();

        let pf = crate::suites::preflight::CatalogPreflightProvider {
            discovery_limits: crate::preflight::discovery::DiscoveryLimits::default(),
            run_preflight_on_bundle: false,
        };
        let bundle = pf.run(thread_id, question, "ask", sctx).await.discovery;

        let registry = Self::build_tools(sctx)?;
        let thread_store = ThreadStore::new(sctx.storage.clone(), sctx.scope.clone(), sctx.keyspace.clone());

        let actx = AgentCtx {
            top_k: 30,
            per_step_timeout_secs: 10,
            max_steps: 50,
            thread_id: Some(thread_id.to_string()),
            progress_tx: None,
            pre_step_tx: None,
            agent_name: Some("ask".to_string()),
            policy: std::sync::Arc::new(SqlValidatedFinalPolicy),
            dataset_candidates: bundle
                .datasets
                .iter()
                .take(8)
                .map(|(ds, sc)| DatasetCandidate {
                    dataset_id: ds.clone(),
                    score: *sc,
                })
                .collect(),
            llm: sctx.llm.clone(),
            storage: sctx.storage.clone(),
            scope: sctx.scope.clone(),
            keyspace: sctx.keyspace.clone(),
            dbt: sctx.dbt.clone(),
            vector: sctx.vector.clone(),
            thread_store: Some(thread_store),
        };

        let mut frames: Vec<FlowFrame> = Vec::new();
        match Agent::run_until_block(&registry, &actx, &sys, &tools_card, question).await {
            Ok(RunOutcome::Final { thread_id: _tid, result }) => {
                frames.push(FlowFrame::Final {
                    answer: result.answer,
                    sql: result.sql,
                });
            }
            Ok(RunOutcome::AwaitUser { thread_id: _tid, prompt }) => {
                frames.push(FlowFrame::AwaitUser { prompt });
            }
            Ok(RunOutcome::AwaitApproval { thread_id: _tid, prompt }) => {
                frames.push(FlowFrame::AwaitUser { prompt });
            }
            Err(e) => return Err(e),
        }
        Ok(frames)
    }
}

#[async_trait]
impl Suite for SkipprAskSuite {
    fn id(&self) -> &'static str {
        "skippr_ask"
    }

    async fn handle_new(
        &self,
        thread_id: &str,
        question: &str,
        agent_type: &str,
        _ctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String> {
        Self::validate_agent_type(agent_type)?;
        Self::run_ask(thread_id, question, _ctx).await
    }

    async fn handle_open(
        &self,
        thread_id: &str,
        question: &str,
        agent_type: &str,
        _ctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String> {
        Self::validate_agent_type(agent_type)?;
        Self::run_ask(thread_id, question, _ctx).await
    }

    async fn handle_user(
        &self,
        thread_id: &str,
        text: &str,
        agent_type: &str,
        _ctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String> {
        Self::validate_agent_type(agent_type)?;
        Self::run_ask(thread_id, text, _ctx).await
    }
}

