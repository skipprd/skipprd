use async_trait::async_trait;
use datafusion::prelude::SessionContext;
use serde_json::json;

use crate::flows::adapter::FlowFrame;
use crate::qa::agent::{Agent, AgentCtx, DatasetCandidate, RunOutcome};
use crate::qa::tools::{Tool, ToolRegistry};
use crate::suites::{Suite, SuiteCtx};
use crate::suites::preflight::PreflightProvider;

pub struct SkipprModelSuite;

impl SkipprModelSuite {
    fn validate_agent_type(agent_type: &str) -> Result<(), String> {
        if agent_type != "model" && agent_type != "cleanse" {
            return Err(format!(
                "invalid agent_type '{}' for suite 'skippr_model' (expected 'model' or 'cleanse')",
                agent_type
            ));
        }
        Ok(())
    }

    fn inject_agent_question(agent_type: &str, question: &str) -> String {
        if agent_type == "model" {
            // Keep existing intent from ws::agent_runner::inject_agent_question, but suite-owned.
            format!(
                "Modeling goal: {}.\n\
                 Act as a proactive DBT Engineer with strong business domain focus.\n\
                 - Resolve datasets; if schema is empty, call sql_register on candidates and proceed anyway with minimal staging models using {{ source('<pipeline>','<namespace>') }}.\n\
                 - Search DBT examples (search_dbt_examples) and adopt conventions from the top match.\n\
                 - Choose artifact type automatically (default DBT model). For project scaffolding, DO NOT build piece‑meal or ask per‑artifact approvals. Produce a consolidated batch of initial artifacts (staging/core/tests/docs) and save them in ONE call to approve_and_save_artifact_batch.\n\
                 - Validate with dbt_validate when available; if unavailable, proceed without blocking.\n\
                 - Ask the user only when confidence is very low (≤0.4) and only for concrete details; after any clarification, write a considered, sentient update from a fastidious custodian of data governance via catalog_note (preview if material).",
                question
            )
        } else {
            question.to_string()
        }
    }

    fn build_tools(agent_type: &str, ctx: &SessionContext) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        match agent_type {
            "cleanse" => {
                use crate::qa::tools::{
                    artifacts::ArtifactsTool,
                    sql_run::SqlRunTool,
                    sql_sample::SqlSampleTool,
                    sql_schema::SqlSchemaTool,
                    sql_stats::SqlStatsTool,
                    vect_query::VectQueryTool,
                };
                registry.register(SqlRunTool { ctx: ctx.clone() });
                registry.register(SqlSchemaTool { ctx: ctx.clone() });
                registry.register(SqlStatsTool);
                registry.register(SqlSampleTool { ctx: ctx.clone() });
                registry.register(VectQueryTool);
                registry.register(ArtifactsTool);
            }
            _ => {
                // model
                use crate::qa::tools::{
                    approve_save::ApproveAndSaveArtifactTool,
                    approve_save_batch::ApproveAndSaveArtifactBatchTool,
                    artifacts::ArtifactsTool,
                    ask_approval::AskApprovalTool,
                    ask_user::AskUserTool,
                    catalog_note::CatalogNoteTool,
                    dbt_examples::SearchDbtExamplesTool,
                    dbt_validate::DbtValidateTool,
                    sql_register::SqlRegisterTool,
                    sql_run::SqlRunTool,
                    sql_sample::SqlSampleTool,
                    sql_schema::SqlSchemaTool,
                    sql_stats::SqlStatsTool,
                    vect_query::VectQueryTool,
                };
                registry.register(SqlRunTool { ctx: ctx.clone() });
                registry.register(SqlSchemaTool { ctx: ctx.clone() });
                registry.register(SqlStatsTool);
                registry.register(SqlSampleTool { ctx: ctx.clone() });
                registry.register(VectQueryTool);
                registry.register(AskUserTool);
                registry.register(AskApprovalTool);
                registry.register(ApproveAndSaveArtifactTool);
                registry.register(ApproveAndSaveArtifactBatchTool);
                registry.register(SearchDbtExamplesTool);
                registry.register(DbtValidateTool);
                registry.register(SqlRegisterTool);
                registry.register(CatalogNoteTool);
                registry.register(ArtifactsTool);
            }
        }
        registry
    }

    async fn run_cleanse(thread_id: &str, question: &str) -> Result<Vec<FlowFrame>, String> {
        let sys = crate::prompts::prompts_shared::with_time_context(crate::qa::prompts::system_prompt());
        let tools_card = crate::qa::prompts::tool_card();

        let ctx_df = crate::ws::agent_runner::get_or_create_thread_ctx(thread_id);

        let pf = crate::suites::preflight::CatalogPreflightProvider {
            discovery_limits: crate::flows::discovery::DiscoveryLimits::default(),
            run_preflight_on_bundle: false,
        };
        let bundle = pf.run(thread_id, question, "cleanse", &ctx_df).await.discovery;

        let mut pairs: Vec<(String, String)> = bundle
            .datasets
            .iter()
            .map(|(p, ns, _)| (p.clone(), ns.clone()))
            .collect();
        crate::flows::util::dedup_pairs(&mut pairs);
        crate::ws::agent_runner::pre_register_selected_namespaces(&ctx_df, &pairs).await;

        let registry = Self::build_tools("cleanse", &ctx_df);

        let actx = AgentCtx {
            top_k: 30,
            per_step_timeout_secs: 10,
            max_steps: 50,
            thread_id: Some(thread_id.to_string()),
            progress_tx: None,
            pre_step_tx: None,
            agent_name: Some("cleanse".to_string()),
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
                frames.push(FlowFrame::AwaitUser { prompt });
            }
            Err(e) => return Err(e),
        }
        Ok(frames)
    }

    async fn run_model(thread_id: &str, question: &str) -> Result<Vec<FlowFrame>, String> {
        let sys = crate::prompts::prompts_shared::with_time_context(crate::prompts::model::system_prompt());
        let tools_card = crate::prompts::model::tool_card();

        let ctx_df = crate::ws::agent_runner::get_or_create_thread_ctx(thread_id);

        let pf = crate::suites::preflight::CatalogPreflightProvider {
            discovery_limits: crate::flows::discovery::DiscoveryLimits::default(),
            run_preflight_on_bundle: true,
        };
        // Liberal parallel discovery (datasets, schemas, samples) + preflight (intent/decision)
        let bundle = pf.run(thread_id, question, "model", &ctx_df).await.discovery;

        let mut pairs: Vec<(String, String)> = bundle
            .datasets
            .iter()
            .map(|(p, ns, _)| (p.clone(), ns.clone()))
            .collect();
        crate::flows::util::dedup_pairs(&mut pairs);
        crate::ws::agent_runner::pre_register_selected_namespaces(&ctx_df, &pairs).await;

        let registry = Self::build_tools("model", &ctx_df);

        let actx = AgentCtx {
            top_k: 30,
            per_step_timeout_secs: 10,
            max_steps: 50,
            thread_id: Some(thread_id.to_string()),
            progress_tx: None,
            pre_step_tx: None,
            agent_name: Some("model".to_string()),
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

        let question2 = Self::inject_agent_question("model", question);

        let mut frames: Vec<FlowFrame> = Vec::new();
        match Agent::run_until_block(&registry, &actx, &sys, &tools_card, &question2).await {
            Ok(RunOutcome::Final { thread_id: _tid, result }) => {
                // enforce no SQL in model finals
                frames.push(FlowFrame::Final {
                    answer: result.answer,
                    sql: None,
                });

                // After artifact save, validate DBT project from S3 and refresh compiled registrations
                let store = crate::qa::session::ThreadStore::new();
                if let Some(log) = store.get(thread_id).await {
                    let mut pipeline_opt: Option<String> = None;
                    for step in log.steps.iter().rev() {
                        if step.action == "artifact_saved" {
                            if let Some(p) = step.args.get("pipeline").and_then(|x| x.as_str()) {
                                if !p.is_empty() {
                                    pipeline_opt = Some(p.to_string());
                                }
                            }
                            break;
                        }
                    }

                    // Fallback: if no artifact was saved, scaffold a full DBT project from discovered datasets
                    if pipeline_opt.is_none() {
                        if let Some((p0, _, _)) = bundle.datasets.first() {
                            let primary = p0.clone();
                            let namespaces: Vec<String> = bundle
                                .datasets
                                .iter()
                                .filter(|(p, _, _)| p == &primary)
                                .map(|(_, ns, _)| ns.clone())
                                .collect();
                            if !namespaces.is_empty() {
                                match crate::qa::dbt::scaffold_full_project(&primary, &namespaces).await {
                                    Ok(keys) => {
                                        let _ = store
                                            .append_step(
                                                thread_id,
                                                crate::qa::session::ThreadStep {
                                                    action: "dbt_scaffold".to_string(),
                                                    args: json!({
                                                        "pipeline": primary,
                                                        "namespaces": namespaces,
                                                        "files": keys
                                                    }),
                                                    observation: json!({"ok": true}),
                                                    ts: chrono::Utc::now().to_rfc3339(),
                                                    agent: Some("model".to_string()),
                                                },
                                            )
                                            .await;
                                        pipeline_opt = Some(primary);
                                    }
                                    Err(e) => {
                                        let _ = store
                                            .append_step(
                                                thread_id,
                                                crate::qa::session::ThreadStep {
                                                    action: "dbt_scaffold".to_string(),
                                                    args: json!({}),
                                                    observation: json!({"ok": false, "error": e}),
                                                    ts: chrono::Utc::now().to_rfc3339(),
                                                    agent: Some("model".to_string()),
                                                },
                                            )
                                            .await;
                                    }
                                }
                            }
                        }
                    }

                    if let Some(pipeline) = pipeline_opt {
                        let tenant = crate::helpers::configuration::Config::get_tenant();
                        let workspace = crate::helpers::configuration::Config::get_workspace_name();
                        let s3_prefix = format!("{}/{}/{}/dbt/", tenant, workspace, pipeline);

                        let validate_tool = crate::qa::tools::dbt_validate::DbtValidateTool;
                        let args = json!({
                            "project_name": format!("{}_project", pipeline.replace('/', "_")),
                            "s3_prefix": s3_prefix,
                            "target": "datafusion",
                            "build": true
                        });
                        let actx2 = AgentCtx {
                            thread_id: Some(thread_id.to_string()),
                            ..actx.clone()
                        };
                        match validate_tool.call(args, &actx2).await {
                            Ok(obs) => {
                                let _ = store
                                    .append_step(
                                        thread_id,
                                        crate::qa::session::ThreadStep {
                                            action: "dbt_validate".to_string(),
                                            args: json!({
                                                "s3_prefix": format!("{}/{}/{}/dbt/", tenant, workspace, pipeline)
                                            }),
                                            observation: obs,
                                            ts: chrono::Utc::now().to_rfc3339(),
                                            agent: Some("model".to_string()),
                                        },
                                    )
                                    .await;
                            }
                            Err(e) => {
                                let _ = store
                                    .append_step(
                                        thread_id,
                                        crate::qa::session::ThreadStep {
                                            action: "dbt_validate".to_string(),
                                            args: json!({
                                                "s3_prefix": format!("{}/{}/{}/dbt/", tenant, workspace, pipeline)
                                            }),
                                            observation: json!({"ok": false, "error": e}),
                                            ts: chrono::Utc::now().to_rfc3339(),
                                            agent: Some("model".to_string()),
                                        },
                                    )
                                    .await;
                            }
                        }

                        // Refresh compiled DBT views (compiled-only registration)
                        let _ = crate::sql::tables::register_dbt_models(&ctx_df).await;
                    }
                }
            }
            Ok(RunOutcome::AwaitUser { thread_id: _tid, prompt }) => {
                frames.push(FlowFrame::AwaitUser { prompt });
            }
            Ok(RunOutcome::AwaitApproval { thread_id: _tid, prompt }) => {
                frames.push(FlowFrame::AwaitApproval { prompt });
            }
            Err(e) => return Err(e),
        }

        Ok(frames)
    }
}

#[async_trait]
impl Suite for SkipprModelSuite {
    fn id(&self) -> &'static str {
        "skippr_model"
    }

    async fn handle_new(
        &self,
        thread_id: &str,
        question: &str,
        agent_type: &str,
        _ctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String> {
        Self::validate_agent_type(agent_type)?;
        match agent_type {
            "cleanse" => Self::run_cleanse(thread_id, question).await,
            _ => Self::run_model(thread_id, question).await,
        }
    }

    async fn handle_open(
        &self,
        thread_id: &str,
        question: &str,
        agent_type: &str,
        _ctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String> {
        Self::validate_agent_type(agent_type)?;
        match agent_type {
            "cleanse" => Self::run_cleanse(thread_id, question).await,
            _ => Self::run_model(thread_id, question).await,
        }
    }

    async fn handle_user(
        &self,
        thread_id: &str,
        text: &str,
        agent_type: &str,
        _ctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String> {
        Self::validate_agent_type(agent_type)?;
        match agent_type {
            "cleanse" => Self::run_cleanse(thread_id, text).await,
            _ => Self::run_model(thread_id, text).await,
        }
    }
}

