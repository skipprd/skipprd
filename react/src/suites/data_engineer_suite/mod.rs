use async_trait::async_trait;
use serde_json::json;

use crate::agent::{Agent, AgentCtx, RunOutcome};
use crate::flow_frame::FlowFrame;
use crate::session::ThreadStore;
use crate::suites::preflight::PreflightProvider;
use crate::suites::skippr_shared::policy_sql_validated::SqlValidatedPolicy;
use crate::suites::skippr_shared::types::DatasetCandidate;
use crate::suites::{Suite, SuiteCtx};
use crate::tools::{Tool, ToolRegistry};

pub struct DataEngineerSuite;

pub mod prompts;
pub mod tools;

impl DataEngineerSuite {
    fn validate_agent_type(agent_type: &str) -> Result<(), String> {
        match agent_type {
            "ask" | "model" | "cleanse" => Ok(()),
            _ => Err(format!(
                "invalid agent_type '{}' for suite 'data_engineer' (expected 'ask' | 'model' | 'cleanse')",
                agent_type
            )),
        }
    }

    fn inject_model_question(question: &str) -> String {
        format!(
            "Modeling goal: {}.\n\
             Act as a proactive DBT Engineer with strong business domain focus.\n\
             - Resolve datasets; if schema is empty, call sql_register on candidates and proceed anyway with minimal staging models using {{ source('<project_id>','<dataset_id>') }} (dataset_id is `catalog.db.table`).\n\
             - Search DBT examples (search_dbt_examples) and adopt conventions from the top match.\n\
             - Choose artifact type automatically (default DBT model). For project scaffolding, DO NOT build piece‑meal or ask per‑artifact approvals. Produce a consolidated batch of initial artifacts (staging/core/tests/docs) and save them in ONE call to approve_and_save_artifact_batch.\n\
             - Validate with dbt_validate when available; if unavailable, proceed without blocking.\n\
             - Ask the user only when confidence is very low (≤0.4) and only for concrete details; after any clarification, write a considered, sentient update from a fastidious custodian of data governance via catalog_note (preview if material).",
            question
        )
    }

    fn build_tools(agent_type: &str, sctx: &SuiteCtx) -> Result<ToolRegistry, String> {
        use crate::suites::shared::tools::{
            artifacts::ArtifactsTool,
            sql_run::SqlRunTool,
            sql_sample::SqlSampleTool,
            sql_schema::SqlSchemaTool,
            sql_stats::SqlStatsTool,
            vect_query::VectQueryTool,
        };

        let mut registry = ToolRegistry::new();

        let query = sctx
            .query
            .as_ref()
            .ok_or_else(|| "query provider missing".to_string())?
            .clone();

        // Shared analytics tools
        registry.register(SqlRunTool { query: query.clone() });
        registry.register(SqlSchemaTool {
            query: query.clone(),
            datasets: sctx.datasets.clone(),
            catalog: sctx.catalog.clone(),
        });
        registry.register(SqlStatsTool {
            catalog: sctx.catalog.clone(),
            datasets: sctx.datasets.clone(),
        });
        registry.register(SqlSampleTool { query: query.clone() });
        registry.register(VectQueryTool);

        match agent_type {
            // cleanse uses only the shared SQL/vect tools + artifacts browsing
            "cleanse" => {
                registry.register(ArtifactsTool);
            }
            // ask uses shared tools + user/approval interrupts + artifacts
            "ask" => {
                registry.register(tools::ask_user::AskUserTool);
                registry.register(tools::ask_approval::AskApprovalTool);
                registry.register(ArtifactsTool);
            }
            // model uses ask tools + artifact authoring + dbt helpers + artifacts
            _ => {
                registry.register(tools::ask_user::AskUserTool);
                registry.register(tools::ask_approval::AskApprovalTool);
                registry.register(tools::approve_save::ApproveAndSaveArtifactTool);
                registry.register(tools::approve_save_batch::ApproveAndSaveArtifactBatchTool);
                registry.register(tools::dbt_examples::SearchDbtExamplesTool);
                registry.register(tools::dbt_validate::DbtValidateTool);
                registry.register(tools::sql_register::SqlRegisterTool);
                registry.register(tools::catalog_note::CatalogNoteTool);
                registry.register(ArtifactsTool);
            }
        }

        Ok(registry)
    }

    async fn run_ask(thread_id: &str, question: &str, sctx: &SuiteCtx) -> Result<Vec<FlowFrame>, String> {
        let sys = crate::util::time_context::with_time_context(prompts::ask_system_prompt());
        let tools_card = prompts::ask_tool_card();

        let pf = crate::suites::preflight::CatalogPreflightProvider {
            discovery_limits: crate::preflight::discovery::DiscoveryLimits::default(),
            run_preflight_on_bundle: false,
        };
        let bundle = pf.run(thread_id, question, "ask", sctx).await.discovery;

        let registry = Self::build_tools("ask", sctx)?;
        let thread_store = ThreadStore::new(sctx.storage.clone(), sctx.scope.clone(), sctx.keyspace.clone());

        let actx = AgentCtx {
            top_k: 30,
            per_step_timeout_secs: 10,
            max_steps: 50,
            thread_id: Some(thread_id.to_string()),
            progress_tx: None,
            pre_step_tx: None,
            agent_name: Some("ask".to_string()),
            policy: std::sync::Arc::new(SqlValidatedPolicy {
                dataset_candidates: bundle
                    .datasets
                    .iter()
                    .take(8)
                    .map(|(ds, sc)| DatasetCandidate {
                        dataset_id: ds.clone(),
                        score: *sc,
                    })
                    .collect(),
                ..SqlValidatedPolicy::default()
            }),
            llm: sctx.llm.clone(),
            storage: sctx.storage.clone(),
            scope: sctx.scope.clone(),
            keyspace: sctx.keyspace.clone(),
            dbt: sctx.dbt.clone(),
            vector: sctx.vector.clone(),
            thread_store: Some(thread_store),
        };

        match Agent::run_until_block(&registry, &actx, &sys, &tools_card, question).await {
            Ok(RunOutcome::Final { thread_id: _tid, result }) => Ok(vec![FlowFrame::Final {
                answer: result.answer,
                sql: result.sql,
            }]),
            Ok(RunOutcome::AwaitUser { thread_id: _tid, prompt }) => Ok(vec![FlowFrame::AwaitUser { prompt }]),
            Ok(RunOutcome::AwaitApproval { thread_id: _tid, prompt }) => Ok(vec![FlowFrame::AwaitApproval { prompt }]),
            Err(e) => Err(e),
        }
    }

    async fn run_cleanse(thread_id: &str, question: &str, sctx: &SuiteCtx) -> Result<Vec<FlowFrame>, String> {
        let sys = crate::util::time_context::with_time_context(prompts::cleanse_system_prompt());
        let tools_card = prompts::cleanse_tool_card();

        let pf = crate::suites::preflight::CatalogPreflightProvider {
            discovery_limits: crate::preflight::discovery::DiscoveryLimits::default(),
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
            policy: std::sync::Arc::new(SqlValidatedPolicy {
                dataset_candidates: bundle
                    .datasets
                    .iter()
                    .take(8)
                    .map(|(ds, sc)| DatasetCandidate {
                        dataset_id: ds.clone(),
                        score: *sc,
                    })
                    .collect(),
                ..SqlValidatedPolicy::default()
            }),
            llm: sctx.llm.clone(),
            storage: sctx.storage.clone(),
            scope: sctx.scope.clone(),
            keyspace: sctx.keyspace.clone(),
            dbt: sctx.dbt.clone(),
            vector: sctx.vector.clone(),
            thread_store: Some(thread_store),
        };

        match Agent::run_until_block(&registry, &actx, &sys, &tools_card, question).await {
            Ok(RunOutcome::Final { thread_id: _tid, result }) => Ok(vec![FlowFrame::Final {
                answer: result.answer,
                sql: result.sql,
            }]),
            Ok(RunOutcome::AwaitUser { thread_id: _tid, prompt }) => Ok(vec![FlowFrame::AwaitUser { prompt }]),
            Ok(RunOutcome::AwaitApproval { thread_id: _tid, prompt }) => Ok(vec![FlowFrame::AwaitApproval { prompt }]),
            Err(e) => Err(e),
        }
    }

    async fn run_model(thread_id: &str, question: &str, sctx: &SuiteCtx) -> Result<Vec<FlowFrame>, String> {
        let sys = crate::util::time_context::with_time_context(prompts::model_system_prompt());
        let tools_card = prompts::model_tool_card();

        let pf = crate::suites::preflight::CatalogPreflightProvider {
            discovery_limits: crate::preflight::discovery::DiscoveryLimits::default(),
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
            policy: std::sync::Arc::new(SqlValidatedPolicy {
                dataset_candidates: bundle
                    .datasets
                    .iter()
                    .take(8)
                    .map(|(ds, sc)| DatasetCandidate {
                        dataset_id: ds.clone(),
                        score: *sc,
                    })
                    .collect(),
                ..SqlValidatedPolicy::default()
            }),
            llm: sctx.llm.clone(),
            storage: sctx.storage.clone(),
            scope: sctx.scope.clone(),
            keyspace: sctx.keyspace.clone(),
            dbt: sctx.dbt.clone(),
            vector: sctx.vector.clone(),
            thread_store: Some(thread_store.clone()),
        };

        let question2 = Self::inject_model_question(question);

        match Agent::run_until_block(&registry, &actx, &sys, &tools_card, &question2).await {
            Ok(RunOutcome::Final { thread_id: _tid, result }) => {
                // Modeling suite returns answer-only; SQL is always null in the final.
                let frames = vec![FlowFrame::Final {
                    answer: result.answer,
                    sql: None,
                }];

                // Post-run: scaffold dbt project + validate (preserve existing behavior).
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
                        if bundle.datasets.first().is_some() {
                            let dataset_ids: Vec<String> = bundle
                                .datasets
                                .iter()
                                .map(|(ds, _)| ds.clone())
                                .collect();
                            if !dataset_ids.is_empty() {
                                let dbt = sctx
                                    .dbt
                                    .as_ref()
                                    .ok_or_else(|| "dbt provider missing".to_string())?;
                                match dbt.scaffold_full_project(&sctx.scope, &dataset_ids).await {
                                    Ok(keys) => {
                                        let _ = store
                                            .append_step(
                                                thread_id,
                                                crate::session::ThreadStep {
                                                    action: "dbt_scaffold".to_string(),
                                                    args: json!({
                                                        "project_id": sctx.scope.project_id,
                                                        "dataset_ids": dataset_ids,
                                                        "files": keys
                                                    }),
                                                    observation: json!({"ok": true}),
                                                    ts: chrono::Utc::now().to_rfc3339(),
                                                    agent: Some("model".to_string()),
                                                },
                                            )
                                            .await;
                                        pipeline_opt = Some(sctx.scope.project_id.clone());
                                    }
                                    Err(e) => {
                                        let _ = store
                                            .append_step(
                                                thread_id,
                                                crate::session::ThreadStep {
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
                        let validate_tool = tools::dbt_validate::DbtValidateTool;
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
                                        crate::session::ThreadStep {
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
                                        crate::session::ThreadStep {
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
                    }
                }

                Ok(frames)
            }
            Ok(RunOutcome::AwaitUser { thread_id: _tid, prompt }) => Ok(vec![FlowFrame::AwaitUser { prompt }]),
            Ok(RunOutcome::AwaitApproval { thread_id: _tid, prompt }) => Ok(vec![FlowFrame::AwaitApproval { prompt }]),
            Err(e) => Err(e),
        }
    }
}

#[async_trait]
impl Suite for DataEngineerSuite {
    fn id(&self) -> &'static str {
        "data_engineer"
    }

    async fn handle_new(
        &self,
        thread_id: &str,
        question: &str,
        agent_type: &str,
        ctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String> {
        Self::validate_agent_type(agent_type)?;
        match agent_type {
            "model" => Self::run_model(thread_id, question, ctx).await,
            "cleanse" => Self::run_cleanse(thread_id, question, ctx).await,
            _ => Self::run_ask(thread_id, question, ctx).await,
        }
    }

    async fn handle_open(
        &self,
        thread_id: &str,
        question: &str,
        agent_type: &str,
        ctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String> {
        Self::validate_agent_type(agent_type)?;
        match agent_type {
            "model" => Self::run_model(thread_id, question, ctx).await,
            "cleanse" => Self::run_cleanse(thread_id, question, ctx).await,
            _ => Self::run_ask(thread_id, question, ctx).await,
        }
    }

    async fn handle_user(
        &self,
        thread_id: &str,
        text: &str,
        agent_type: &str,
        ctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String> {
        Self::validate_agent_type(agent_type)?;
        match agent_type {
            "model" => Self::run_model(thread_id, text, ctx).await,
            "cleanse" => Self::run_cleanse(thread_id, text, ctx).await,
            _ => Self::run_ask(thread_id, text, ctx).await,
        }
    }
}

