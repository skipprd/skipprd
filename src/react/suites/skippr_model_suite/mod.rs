use async_trait::async_trait;
use serde_json::json;

use crate::react::flow_frame::FlowFrame;
use crate::react::agent::{Agent, AgentCtx, DatasetCandidate, RunOutcome};
use crate::react::tools::{Tool, ToolRegistry};
use crate::react::suites::preflight::PreflightProvider;
use crate::react::suites::{Suite, SuiteCtx};
use crate::react::session::ThreadStore;

pub struct SkipprModelSuite;

pub mod tools;
pub mod prompts;
pub mod flows;

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

    fn build_tools(agent_type: &str, sctx: &SuiteCtx) -> Result<ToolRegistry, String> {
        let mut registry = ToolRegistry::new();
        let query = sctx
            .query
            .as_ref()
            .ok_or_else(|| "query provider missing".to_string())?
            .clone();
        match agent_type {
            "cleanse" => {
                use crate::react::suites::shared::tools::{
                    artifacts::ArtifactsTool,
                    sql_run::SqlRunTool,
                    sql_sample::SqlSampleTool,
                    sql_schema::SqlSchemaTool,
                    sql_stats::SqlStatsTool,
                    vect_query::VectQueryTool,
                };
                registry.register(SqlRunTool { query: query.clone() });
                registry.register(SqlSchemaTool { query: query.clone(), datasets: sctx.datasets.clone(), catalog: sctx.catalog.clone() });
                registry.register(SqlStatsTool { catalog: sctx.catalog.clone(), datasets: sctx.datasets.clone() });
                registry.register(SqlSampleTool { query: query.clone() });
                registry.register(VectQueryTool);
                registry.register(ArtifactsTool);
            }
            _ => {
                use crate::react::suites::shared::tools::{
                    artifacts::ArtifactsTool,
                    sql_run::SqlRunTool,
                    sql_sample::SqlSampleTool,
                    sql_schema::SqlSchemaTool,
                    sql_stats::SqlStatsTool,
                    vect_query::VectQueryTool,
                };
                use crate::react::suites::skippr_ask_suite::tools::{ask_approval::AskApprovalTool, ask_user::AskUserTool};
                use crate::react::suites::skippr_model_suite::tools::{
                    approve_save::ApproveAndSaveArtifactTool,
                    approve_save_batch::ApproveAndSaveArtifactBatchTool,
                    catalog_note::CatalogNoteTool,
                    dbt_examples::SearchDbtExamplesTool,
                    dbt_validate::DbtValidateTool,
                    sql_register::SqlRegisterTool,
                };
                registry.register(SqlRunTool { query: query.clone() });
                registry.register(SqlSchemaTool { query: query.clone(), datasets: sctx.datasets.clone(), catalog: sctx.catalog.clone() });
                registry.register(SqlStatsTool { catalog: sctx.catalog.clone(), datasets: sctx.datasets.clone() });
                registry.register(SqlSampleTool { query: query.clone() });
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
        Ok(registry)
    }

    pub(crate) async fn run_cleanse(thread_id: &str, question: &str, sctx: &SuiteCtx) -> Result<Vec<FlowFrame>, String> {
        let sys = crate::react::util::time_context::with_time_context(crate::react::prompts::ask_legacy::system_prompt());
        let tools_card = crate::react::prompts::ask_legacy::tool_card();

        let pf = crate::react::suites::preflight::CatalogPreflightProvider {
            discovery_limits: crate::react::preflight::discovery::DiscoveryLimits::default(),
            run_preflight_on_bundle: false,
        };
        let bundle = pf.run(thread_id, question, "cleanse", sctx).await.discovery;

        let registry = Self::build_tools("cleanse", sctx)?;
        let thread_store = ThreadStore::new(sctx.storage.clone(), sctx.scope.clone(), sctx.keyspace.clone());

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

    pub(crate) async fn run_model(thread_id: &str, question: &str, sctx: &SuiteCtx) -> Result<Vec<FlowFrame>, String> {
        let sys = crate::react::util::time_context::with_time_context(crate::react::suites::skippr_model_suite::prompts::system_prompt());
        let tools_card = crate::react::suites::skippr_model_suite::prompts::tool_card();

        let pf = crate::react::suites::preflight::CatalogPreflightProvider {
            discovery_limits: crate::react::preflight::discovery::DiscoveryLimits::default(),
            run_preflight_on_bundle: true,
        };
        let bundle = pf.run(thread_id, question, "model", sctx).await.discovery;

        let registry = Self::build_tools("model", sctx)?;
        let thread_store = ThreadStore::new(sctx.storage.clone(), sctx.scope.clone(), sctx.keyspace.clone());

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
            llm: sctx.llm.clone(),
            storage: sctx.storage.clone(),
            scope: sctx.scope.clone(),
            keyspace: sctx.keyspace.clone(),
            dbt: sctx.dbt.clone(),
            vector: sctx.vector.clone(),
            thread_store: Some(thread_store.clone()),
        };

        let question2 = Self::inject_agent_question("model", question);

        let mut frames: Vec<FlowFrame> = Vec::new();
        match Agent::run_until_block(&registry, &actx, &sys, &tools_card, &question2).await {
            Ok(RunOutcome::Final { thread_id: _tid, result }) => {
                frames.push(FlowFrame::Final {
                    answer: result.answer,
                    sql: None,
                });

                let store = thread_store.clone();
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
                                let dbt = sctx
                                    .dbt
                                    .as_ref()
                                    .ok_or_else(|| "dbt provider missing".to_string())?;
                                match dbt.scaffold_full_project(&sctx.scope, &primary, &namespaces).await {
                                    Ok(keys) => {
                                        let _ = store
                                            .append_step(
                                                thread_id,
                                                crate::react::session::ThreadStep {
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
                                                crate::react::session::ThreadStep {
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
                        let validate_tool = crate::react::suites::skippr_model_suite::tools::dbt_validate::DbtValidateTool;
                        let args = json!({
                            "project_name": format!("{}_project", pipeline.replace('/', "_")),
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
                                        crate::react::session::ThreadStep {
                                            action: "dbt_validate".to_string(),
                                            args: json!({ "pipeline": pipeline }),
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
                                        crate::react::session::ThreadStep {
                                            action: "dbt_validate".to_string(),
                                            args: json!({ "pipeline": pipeline }),
                                            observation: json!({"ok": false, "error": e}),
                                            ts: chrono::Utc::now().to_rfc3339(),
                                            agent: Some("model".to_string()),
                                        },
                                    )
                                    .await;
                            }
                        }

                        // No sqlrt/DataFusion registration in engine-agnostic ReAct.
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
        sctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String> {
        Self::validate_agent_type(agent_type)?;
        match agent_type {
            "cleanse" => Self::run_cleanse(thread_id, question, sctx).await,
            _ => Self::run_model(thread_id, question, sctx).await,
        }
    }

    async fn handle_open(
        &self,
        thread_id: &str,
        question: &str,
        agent_type: &str,
        sctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String> {
        Self::validate_agent_type(agent_type)?;
        match agent_type {
            "cleanse" => Self::run_cleanse(thread_id, question, sctx).await,
            _ => Self::run_model(thread_id, question, sctx).await,
        }
    }

    async fn handle_user(
        &self,
        thread_id: &str,
        text: &str,
        agent_type: &str,
        sctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String> {
        Self::validate_agent_type(agent_type)?;
        match agent_type {
            "cleanse" => Self::run_cleanse(thread_id, text, sctx).await,
            _ => Self::run_model(thread_id, text, sctx).await,
        }
    }
}

