use async_trait::async_trait;
use serde_json::json;

use crate::agent::{Agent, AgentCtx, RunOutcome};
use crate::flow_frame::FlowFrame;
use crate::session::ThreadStore;
use crate::suites::preflight::PreflightProvider;
use crate::suites::data_engineer_shared::policy_sql_validated::SqlValidatedPolicy;
use crate::suites::data_engineer_shared::types::DatasetCandidate;
use crate::suites::{Suite, SuiteCtx};
use crate::tools::{Tool, ToolRegistry};
use std::collections::HashMap;

pub struct DataEngineerSuite;

pub mod prompts;
pub mod tools;
pub mod dbt_error;

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
             - Do NOT assume table names.\n\
             - If embeddings/vect search yields no candidates, call sql_schema with no args to list tables.\n\
             - Hybrid selection: propose a shortlist of tables (with brief reasons based on schema/stats), then ask_approval to confirm the table list before writing any artifacts.\n\
             - Silver/staging is LLM-authored and iterative: use `staging_model` to author/update staging models (cleansing + nested field extraction) before writing core/gold models.\n\
             - Search DBT examples (search_dbt_examples) and adopt conventions from the top match.\n\
             - Model relationships and flow:\n\
               - Identify join keys (user/profile/account/session/device identifiers) across the approved tables using sql_schema + sql_sample/sql_stats.\n\
               - Identify event time fields and ordering semantics; do NOT assume the timestamp column name.\n\
               - For event-style datasets, prefer building a core/funnel mart that sequences events per entity and computes step completion + step-to-step durations.\n\
               - Add dbt tests (not_null/unique/relationships) for the chosen keys and important timestamps.\n\
             - Batch scaffolding: use approve_and_save_artifact_batch to write MANY files, but keep each call small enough to fit the output limit.\n\
               - Hard cap: <= 20 items per approve_and_save_artifact_batch call.\n\
               - If you need more files, do multiple batch-save tool calls over multiple steps.\n\
               - IMPORTANT: write real DBT project files as kind='file' with explicit paths (e.g. path='dbt_project.yml', 'models/schema.yml', 'models/staging/stg_<table>.sql', 'models/core/...'). Do NOT save dbt_project.yml as a .sql model.\n\
             - After saving artifacts: ALWAYS validate with dbt_validate. If validate fails, iterate (edit artifacts, re-validate) until clean.\n\
             - When validation is clean: call publish_dbt_to_provider to materialize curated relations in the active warehouse provider.\n\
               - If publish returns await_approval: ask the user to approve; on approval, re-run publish_dbt_to_provider with confirm=true.\n\
               - Default materialization is view; if you believe table or incremental is better, propose it with rationale and await approval before changing materializations.\n\
             - Ask the user only when confidence is very low (≤0.4) and only for concrete details; after any clarification, write a considered, sentient update from a fastidious custodian of data governance via catalog_note (preview if material).",
            question
        )
    }

    fn inject_cleanse_question(question: &str) -> String {
        format!(
            "Cleansing goal: {}.\n\
             Act as a proactive DBT Engineer focused on producing a curated silver tier.\n\
             - Prefer DBT models over ad-hoc SQL; author staging models and tests.\n\
             - Use `staging_model` to author/update staging models (cleansing + nested field extraction). Treat user instructions as authoritative constraints.\n\
             - Do NOT assume table names.\n\
             - If embeddings/vect search yields no candidates, call sql_schema with no args to list tables.\n\
             - Hybrid selection: propose a shortlist of tables (with brief reasons based on schema/stats), then ask_approval to confirm the table list before writing any artifacts.\n\
             - Model relationships and flow:\n\
               - Identify join keys (user/profile/account/session/device identifiers) and timestamp fields using sql_schema + sql_sample/sql_stats.\n\
               - Prefer staged normalization (consistent key/timestamp names) to make downstream joins reliable.\n\
               - Add dbt tests (not_null/unique/relationships) for chosen keys and key timestamps.\n\
             - Batch scaffolding: use approve_and_save_artifact_batch to write MANY files, but keep each call small enough to fit the output limit.\n\
               - Hard cap: <= 20 items per approve_and_save_artifact_batch call.\n\
               - If you need more files, do multiple batch-save tool calls over multiple steps.\n\
               - Use kind='file' + explicit paths for dbt_project.yml and YAML.\n\
             - After saving artifacts: ALWAYS validate with dbt_validate. If validate fails, iterate (edit artifacts, re-validate) until clean.\n\
             - When validation is clean: call publish_dbt_to_provider (views by default; propose tables/incremental with rationale and await approval).\n\
             - Use catalog_note to record notable cleansing decisions and assumptions (preview if material).",
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
            // cleanse uses shared tools + authoring/validation/publish loop
            "cleanse" => {
                registry.register(tools::ask_user::AskUserTool);
                registry.register(tools::ask_approval::AskApprovalTool);
                registry.register(tools::approve_save::ApproveAndSaveArtifactTool);
                registry.register(tools::approve_save_batch::ApproveAndSaveArtifactBatchTool);
                registry.register(tools::dbt_examples::SearchDbtExamplesTool);
                registry.register(tools::staging_model::StagingModelTool { datasets: sctx.datasets.clone() });
                registry.register(tools::dbt_validate::DbtValidateTool { datasets: sctx.datasets.clone(), catalog: sctx.catalog.clone() });
                registry.register(tools::publish_dbt_to_provider::PublishDbtToProviderTool { datasets: sctx.datasets.clone(), catalog: sctx.catalog.clone() });
                registry.register(tools::sql_register::SqlRegisterTool);
                registry.register(tools::catalog_note::CatalogNoteTool);
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
                registry.register(tools::staging_model::StagingModelTool { datasets: sctx.datasets.clone() });
                registry.register(tools::dbt_validate::DbtValidateTool { datasets: sctx.datasets.clone(), catalog: sctx.catalog.clone() });
                registry.register(tools::publish_dbt_to_provider::PublishDbtToProviderTool { datasets: sctx.datasets.clone(), catalog: sctx.catalog.clone() });
                registry.register(tools::sql_register::SqlRegisterTool);
                registry.register(tools::catalog_note::CatalogNoteTool);
                registry.register(ArtifactsTool);
            }
        }

        Ok(registry)
    }

    /// If no catalogs/stats exist yet for this scope, build them for all tables first.
    ///
    /// This avoids table-name assumptions and gives the agent a reliable base for shortlist selection.
    async fn ensure_catalog_bootstrap(sctx: &SuiteCtx) {
        let (Some(cat), Some(datasets)) = (sctx.catalog.as_ref(), sctx.datasets.as_ref()) else {
            return;
        };
        // Detect whether any catalog entry already exists (sample a few datasets).
        let mut has_any = false;
        if let Ok(dss) = datasets.list_datasets().await {
            for ds in dss.iter().take(5) {
                let id = ds.fqn();
                if let Ok(Some(_)) = cat.read_catalog(&sctx.scope, &id).await {
                    has_any = true;
                    break;
                }
            }
            if !has_any {
                tracing::info!("data_engineer: no existing catalog found; building catalogs/stats for all datasets");
                let empty: HashMap<String, crate::discover::Metadata> = HashMap::new();
                let _ = cat
                    .build_all_with_progress(&sctx.scope, datasets.as_ref(), &empty, None)
                    .await;
            }
        }
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
            trace_tx: sctx.trace_tx.clone(),
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
            query: sctx.query.clone(),
            dbt: sctx.dbt.clone(),
            vector: sctx.vector.clone(),
            thread_store: Some(thread_store),
            resolved_config: sctx.resolved_config.clone(),
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
        Self::ensure_catalog_bootstrap(sctx).await;
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
            trace_tx: sctx.trace_tx.clone(),
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
            query: sctx.query.clone(),
            dbt: sctx.dbt.clone(),
            vector: sctx.vector.clone(),
            thread_store: Some(thread_store),
            resolved_config: sctx.resolved_config.clone(),
        };

        let mut last_final: Option<crate::session::ThreadResult> = None;
        let mut prompt = Self::inject_cleanse_question(question);
        for attempt in 0..10 {
            match Agent::run_until_block(&registry, &actx, &sys, &tools_card, &prompt).await {
                Ok(RunOutcome::Final { thread_id: _tid, result }) => {
                    last_final = Some(result.clone());

                    // Post-run validate (includes build) so runtime failures feed back into auto-remediation.
                    let validate_tool = tools::dbt_validate::DbtValidateTool { datasets: sctx.datasets.clone(), catalog: sctx.catalog.clone() };
                    let args = json!({
                        "project_name": format!("{}_project", sctx.scope.project_id.replace('/', "_")),
                        "build": true
                    });
                    let obs = validate_tool.call(args, &actx).await.unwrap_or_else(|e| json!({"ok": false, "error": e}));
                    let ok = obs.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
                    let compile_ok = obs.get("compile_ok").and_then(|v| v.as_bool()).unwrap_or(false);
                    let run_ok = obs.get("run_ok").and_then(|v| v.as_bool()).unwrap_or(false);
                    if ok && compile_ok && run_ok {
                        return Ok(vec![FlowFrame::Final { answer: result.answer, sql: result.sql }]);
                    }
                    let errs: Vec<String> = obs
                        .get("errors")
                        .and_then(|v| serde_json::from_value(v.clone()).ok())
                        .unwrap_or_default();
                    let class = dbt_error::classify(&errs);
                    let brief = dbt_error::compact_brief(&errs, 2, 900);
                    if matches!(class, dbt_error::DbtErrorClass::WarehouseConfig) {
                        return Ok(vec![FlowFrame::AwaitUser {
                            prompt: format!(
                                "dbt_validate failed due to a warehouse/AWS configuration issue. Fix the Athena/workgroup/region/credentials and then reply 'continue'.\n\nError summary:\n{}",
                                brief
                            ),
                        }]);
                    }
                    prompt = format!(
                        "Auto-remediation attempt {}: dbt_validate/build failed.\n\nError summary:\n{}\n\nDo NOT ask the user for confirmation/approval. Automatically fix the DBT project:\n- Prefer calling `staging_model` to update staging/silver models (nested fields, cleansing, naming).\n- Use artifacts to edit existing files (preview_diff if helpful).\n- Re-run dbt_validate with build=true.\nRepeat until compile_ok=true AND run_ok=true.",
                        attempt + 1,
                        brief
                    );
                    continue;
                }
                Ok(RunOutcome::AwaitUser { thread_id: _tid, prompt: p }) => {
                    prompt = format!(
                        "AUTO: the agent requested user input:\n{}\n\nAuto-response: proceed with best-effort assumptions and continue. Do NOT ask the user again.",
                        p
                    );
                    continue;
                }
                Ok(RunOutcome::AwaitApproval { thread_id: _tid, prompt: p }) => {
                    prompt = format!(
                        "AUTO: the agent requested approval:\n{}\n\nAuto-response: approved. Proceed without further approval prompts.",
                        p
                    );
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        // Fallback: return last known final if any.
        if let Some(r) = last_final {
            return Ok(vec![FlowFrame::Final { answer: r.answer, sql: r.sql }]);
        }
        Err("cleanse: no outcome".to_string())
    }

    async fn run_model(thread_id: &str, question: &str, sctx: &SuiteCtx) -> Result<Vec<FlowFrame>, String> {
        Self::ensure_catalog_bootstrap(sctx).await;
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
            trace_tx: sctx.trace_tx.clone(),
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
            query: sctx.query.clone(),
            dbt: sctx.dbt.clone(),
            vector: sctx.vector.clone(),
            thread_store: Some(thread_store.clone()),
            resolved_config: sctx.resolved_config.clone(),
        };

        let question2 = Self::inject_model_question(question);
        let mut last_answer: Option<String> = None;
        let mut prompt = question2;
        for attempt in 0..10 {
            match Agent::run_until_block(&registry, &actx, &sys, &tools_card, &prompt).await {
                Ok(RunOutcome::Final { thread_id: _tid, result }) => {
                    last_answer = Some(result.answer.clone());

                    // NOTE: hard cut-over: we do not auto-scaffold templated staging models.
                    // Staging/silver models must be authored via `staging_model` (LLM-driven) and saved as artifacts.

                    // Post-run validate (includes build) so runtime failures feed back into auto-remediation.
                    let validate_tool = tools::dbt_validate::DbtValidateTool { datasets: sctx.datasets.clone(), catalog: sctx.catalog.clone() };
                    let args = json!({
                        "project_name": format!("{}_project", sctx.scope.project_id.replace('/', "_")),
                        "build": true
                    });
                    let obs = validate_tool.call(args, &actx).await.unwrap_or_else(|e| json!({"ok": false, "error": e}));
                    let ok = obs.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
                    let compile_ok = obs.get("compile_ok").and_then(|v| v.as_bool()).unwrap_or(false);
                    let run_ok = obs.get("run_ok").and_then(|v| v.as_bool()).unwrap_or(false);
                    if ok && compile_ok && run_ok {
                        return Ok(vec![FlowFrame::Final { answer: result.answer, sql: None }]);
                    }
                    let errs: Vec<String> = obs
                        .get("errors")
                        .and_then(|v| serde_json::from_value(v.clone()).ok())
                        .unwrap_or_default();
                    let class = dbt_error::classify(&errs);
                    let brief = dbt_error::compact_brief(&errs, 2, 900);
                    if matches!(class, dbt_error::DbtErrorClass::WarehouseConfig) {
                        return Ok(vec![FlowFrame::AwaitUser {
                            prompt: format!(
                                "dbt_validate failed due to a warehouse/AWS configuration issue. Fix the Athena/workgroup/region/credentials and then reply 'continue'.\n\nError summary:\n{}",
                                brief
                            ),
                        }]);
                    }
                    prompt = format!(
                        "Auto-remediation attempt {}: dbt_validate/build failed.\n\nError summary:\n{}\n\nDo NOT ask the user for confirmation/approval. Automatically fix the DBT project:\n- Prefer calling `staging_model` to update staging/silver models (nested fields, cleansing, naming).\n- Use artifacts to edit existing files (preview_diff if helpful).\n- Re-run dbt_validate with build=true.\nRepeat until compile_ok=true AND run_ok=true.",
                        attempt + 1,
                        brief
                    );
                    continue;
                }
                Ok(RunOutcome::AwaitUser { thread_id: _tid, prompt: p }) => {
                    prompt = format!(
                        "AUTO: the agent requested user input:\n{}\n\nAuto-response: proceed with best-effort assumptions and continue. Do NOT ask the user again.",
                        p
                    );
                    continue;
                }
                Ok(RunOutcome::AwaitApproval { thread_id: _tid, prompt: p }) => {
                    prompt = format!(
                        "AUTO: the agent requested approval:\n{}\n\nAuto-response: approved. Proceed without further approval prompts.",
                        p
                    );
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        if let Some(a) = last_answer {
            return Ok(vec![FlowFrame::Final { answer: a, sql: None }]);
        }
        Err("model: no outcome".to_string())
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

