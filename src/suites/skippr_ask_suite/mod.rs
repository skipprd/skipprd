use async_trait::async_trait;
use datafusion::prelude::SessionContext;

use crate::flows::adapter::FlowFrame;
use crate::qa::agent::{Agent, AgentCtx, DatasetCandidate, RunOutcome};
use crate::qa::tools::ToolRegistry;
use crate::suites::{Suite, SuiteCtx};
use crate::suites::preflight::PreflightProvider;

pub struct SkipprAskSuite;

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

    fn build_tools(ctx: &SessionContext) -> ToolRegistry {
        use crate::qa::tools::{
            artifacts::ArtifactsTool,
            ask_approval::AskApprovalTool,
            ask_user::AskUserTool,
            sql_run::SqlRunTool,
            sql_sample::SqlSampleTool,
            sql_schema::SqlSchemaTool,
            sql_stats::SqlStatsTool,
            vect_query::VectQueryTool,
        };

        let mut registry = ToolRegistry::new();
        registry.register(SqlRunTool { ctx: ctx.clone() });
        registry.register(SqlSchemaTool { ctx: ctx.clone() });
        registry.register(SqlStatsTool);
        registry.register(SqlSampleTool { ctx: ctx.clone() });
        registry.register(VectQueryTool);
        registry.register(AskUserTool);
        registry.register(AskApprovalTool);
        registry.register(ArtifactsTool);
        registry
    }

    async fn run_ask(thread_id: &str, question: &str) -> Result<Vec<FlowFrame>, String> {
        // Prompts
        let sys = crate::prompts::prompts_shared::with_time_context(crate::prompts::ask::system_prompt());
        let tools_card = crate::prompts::ask::tool_card();

        // Thread-scoped DF context
        let ctx_df = crate::ws::agent_runner::get_or_create_thread_ctx(thread_id);

        // Preflight (pluggable; default provider uses existing catalog+discovery)
        let pf = crate::suites::preflight::CatalogPreflightProvider {
            discovery_limits: crate::flows::discovery::DiscoveryLimits::default(),
            run_preflight_on_bundle: false,
        };
        let bundle = pf.run(thread_id, question, "ask", &ctx_df).await.discovery;

        // Register datasets selected by discovery into the DF context
        let mut pairs: Vec<(String, String)> = bundle
            .datasets
            .iter()
            .map(|(p, ns, _)| (p.clone(), ns.clone()))
            .collect();
        crate::flows::util::dedup_pairs(&mut pairs);
        crate::ws::agent_runner::pre_register_selected_namespaces(&ctx_df, &pairs).await;

        // Tool registry is suite-owned
        let registry = Self::build_tools(&ctx_df);

        // Agent ctx
        let actx = AgentCtx {
            top_k: 30,
            per_step_timeout_secs: 10,
            max_steps: 50,
            thread_id: Some(thread_id.to_string()),
            progress_tx: None,
            pre_step_tx: None,
            agent_name: Some("ask".to_string()),
            dataset_candidates: bundle
                .datasets
                .iter()
                .take(8)
                .map(|(p, ns, sc)| DatasetCandidate {
                    pipeline: p.clone(),
                    namespace: ns.clone(),
                    score: *sc,
                })
                .collect(),
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
                // Ask suite is read-only; treat as generic await user.
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
        Self::run_ask(thread_id, question).await
    }

    async fn handle_open(
        &self,
        thread_id: &str,
        question: &str,
        agent_type: &str,
        _ctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String> {
        Self::validate_agent_type(agent_type)?;
        Self::run_ask(thread_id, question).await
    }

    async fn handle_user(
        &self,
        thread_id: &str,
        text: &str,
        agent_type: &str,
        _ctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String> {
        Self::validate_agent_type(agent_type)?;
        // For ask suite, run with the latest user text as prompt.
        Self::run_ask(thread_id, text).await
    }
}

