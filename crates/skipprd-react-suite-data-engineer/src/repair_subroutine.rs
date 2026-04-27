use async_trait::async_trait;
use react_core::agent::{Agent, AgentCtx, AgentCtxBuilder};
use react_core::llm::{ChatMessage, ChatRole};
use react_core::session::ThreadStore;
use react_core::suite::{FlowFrame, SuiteCtx};
use react_core::tools::ToolRegistry;
use react_core::workflow::{PhaseExecutor, PhaseOutcome, WorkflowConfig};
use serde::Deserialize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Column contract for one upstream model, extracted from investigation results.
#[derive(Clone, Debug, serde::Serialize, Deserialize, schemars::JsonSchema)]
struct UpstreamModelSchema {
    /// Relative path (e.g. "models/marts/gold_dim_customers.sql").
    model_path: String,
    /// Exact column names exposed by this model's final SELECT.
    columns: Vec<String>,
}

/// Structured diagnosis output from the gather stage.
///
/// Sent to the LLM as an OpenAI Strict Schema so the response is guaranteed to
/// conform (on providers that support it).  Deserialized directly — no
/// string-inside-string payload gymnastics.
#[derive(Clone, Debug, serde::Serialize, Deserialize, schemars::JsonSchema)]
struct GatherDiagnosisV1 {
    /// Root-cause analysis explaining why the dbt model(s) are failing.
    diagnosis: String,
    /// Relative file paths of models that likely need to be fixed.
    affected_files: Vec<String>,
    /// Column schemas of upstream models referenced via ref() by the failing model(s).
    /// Extracted from file contents or sql_schema tool observations.
    upstream_schemas: Vec<UpstreamModelSchema>,
    /// True when ALL original errors (from the Error Context) have been resolved by
    /// prior iterations, even if the latest validation reveals NEW errors (e.g. cascade
    /// failures from previously-skipped downstream models).  Always false on the first
    /// iteration since no fix has been attempted yet.
    original_errors_resolved: bool,
}

use crate::control_flow::DeterministicDbtValidateOnce;
use crate::model_dispatch::ModelDispatch;
use crate::progress_controller::ValidationFailureContext;
use crate::repair_session::{
    ApplyResult, FileOp, GatheredFile, PlannedFix, RepairIteration, RepairSessionLog,
    ValidateOutcome,
};

const GATHER_MAX_STEPS: usize = 14;
const DEFAULT_MAX_ITERATIONS: usize = 5;

struct RepairExecutor<'a> {
    sctx: &'a SuiteCtx,
    thread_id: &'a str,
    dispatch: &'a ModelDispatch,
    session_log: std::sync::Mutex<RepairSessionLog>,
    iteration: AtomicUsize,
    repair_cycle: usize,
    repair_id: String,
}

#[async_trait]
impl<'a> PhaseExecutor for RepairExecutor<'a> {
    async fn execute_turn(&self, _out_frames: &mut Vec<FlowFrame>) -> PhaseOutcome {
        let i = self.iteration.fetch_add(1, Ordering::Relaxed);
        let log_snapshot = self.session_log.lock().unwrap().clone();

        let gathered = match run_gather(
            self.sctx,
            self.thread_id,
            self.dispatch,
            &log_snapshot,
            i,
            self.repair_cycle,
            &self.repair_id,
        )
        .await
        {
            Ok(g) => g,
            Err(e) => return PhaseOutcome::Failed { reason: e },
        };

        if i > 0 && gathered.original_errors_resolved {
            tracing::info!(
                iteration = i,
                "original errors resolved — exiting repair loop, \
                 new errors will be handled by the outer validate cycle"
            );
            return PhaseOutcome::Return(vec![]);
        }

        let fix_plan = match run_reason(self.sctx, self.dispatch, &gathered, &log_snapshot, i).await
        {
            Ok(p) => p,
            Err(e) => return PhaseOutcome::Failed { reason: e },
        };

        let apply_results = apply_fixes(self.sctx, self.thread_id, &fix_plan).await;

        let diagnosis = gathered.diagnosis.clone();
        {
            let mut log = self.session_log.lock().unwrap();
            log.record(i, gathered.files, diagnosis, fix_plan, apply_results);
        }

        let has_mutations = self
            .session_log
            .lock()
            .unwrap()
            .last()
            .map(|it| it.has_mutations())
            .unwrap_or(false);

        if has_mutations {
            let actx = build_tool_ctx(self.sctx, self.thread_id);
            let validate_result = DeterministicDbtValidateOnce::run(&actx, true, false, None).await;

            let (passed, entry_clone) = {
                let mut log = self.session_log.lock().unwrap();
                match validate_result {
                    Ok(contract) if contract.outcome_v2.ok => {
                        log.record_validate(ValidateOutcome {
                            passed: true,
                            error_summary: "all tests passed".into(),
                        });
                    }
                    Ok(contract) => {
                        let error_summary = serde_json::to_string(&contract.observation)
                            .unwrap_or_else(|_| "validation failed".into());
                        let truncated = if error_summary.len() > 4000 {
                            format!("{}…", &error_summary[..4000])
                        } else {
                            error_summary
                        };
                        log.record_validate(ValidateOutcome {
                            passed: false,
                            error_summary: truncated,
                        });
                    }
                    Err(e) => {
                        log.record_validate(ValidateOutcome {
                            passed: false,
                            error_summary: format!("validate execution error: {e}"),
                        });
                    }
                }
                let p = log
                    .last()
                    .and_then(|it| it.validate_outcome.as_ref())
                    .map(|v| v.passed)
                    .unwrap_or(false);
                (p, log.last().cloned())
            };

            if let Some(entry) = entry_clone.as_ref() {
                index_to_vector_store(self.sctx, entry, passed).await;
            }
            if passed {
                crate::phase_plan_lifecycle::refresh_model_plan_grounded_schemas(self.sctx, &actx)
                    .await;
                return PhaseOutcome::Return(vec![]);
            }
        }

        PhaseOutcome::TransitionCommitted
    }

    async fn on_budget_exhausted(&self, _out_frames: &mut Vec<FlowFrame>, _total_steps: usize) {}

    async fn step_count(&self) -> usize {
        self.iteration.load(Ordering::Relaxed)
    }
}

/// Three-stage repair subroutine: Gather → Reason → Apply → Validate.
///
/// Uses the common workflow runner with one turn per iteration. Each iteration
/// accumulates into the `RepairSessionLog` so prompts are never identical
/// and the LLM always sees full history of prior attempts.
pub async fn run_repair(
    sctx: &SuiteCtx,
    _thread_store: &ThreadStore,
    thread_id: &str,
    dispatch: &ModelDispatch,
    error_context: ValidationFailureContext,
    max_iterations: Option<usize>,
    repair_cycle: usize,
) -> Result<Vec<FlowFrame>, String> {
    let max_iters = max_iterations.unwrap_or(DEFAULT_MAX_ITERATIONS);
    let repair_id = uuid::Uuid::new_v4().to_string()[..8].to_string();

    let executor = RepairExecutor {
        sctx,
        thread_id,
        dispatch,
        session_log: std::sync::Mutex::new(RepairSessionLog::new(error_context)),
        iteration: AtomicUsize::new(0),
        repair_cycle,
        repair_id,
    };

    let config = WorkflowConfig {
        max_phase_steps: max_iters,
        max_consecutive_waiting_idle: max_iters,
        max_consecutive_waiting_active: max_iters,
        ..Default::default()
    };

    react_core::workflow::runner::run(&executor, &config).await
}

/// Stage 1: Gather — two-phase approach.
///
/// Phase A: Run an agent with read-only tools to investigate the failure.
///          The agent reads SQL models, YAML schemas, and queries the vector store.
///          We don't care about its completion payload — only about the tool observations
///          it leaves in the thread store.
///
/// Phase B: Make a single structured LLM call (with `GatherDiagnosisV1` JSON schema)
///          using the investigation observations as context.  The response deserialises
///          directly into a Rust struct — no string-inside-string, no fallback parsing.
async fn run_gather(
    sctx: &SuiteCtx,
    thread_id: &str,
    dispatch: &ModelDispatch,
    session_log: &RepairSessionLog,
    iteration: usize,
    repair_cycle: usize,
    repair_id: &str,
) -> Result<GatheredContext, String> {
    // ── Phase A: investigation agent (tool calls) ────────────────────────
    let registry = build_gather_tools(sctx)?;
    let tools_card = gather_tools_card();

    let gather_tid = format!("{thread_id}__gather_c{repair_cycle}-{repair_id}_{iteration}");

    let actx = AgentCtxBuilder::new(
        sctx.llm().clone(),
        sctx.storage().clone(),
        sctx.scope().clone(),
        sctx.keyspace().clone(),
        Arc::new(react_core::agent::DefaultPolicy),
    )
    .top_k(crate::env_util::DEFAULT_TOP_K)
    .per_step_timeout_secs(10)
    .max_steps(GATHER_MAX_STEPS)
    .thread_id(&gather_tid)
    .trace_tx(sctx.trace_tx().clone())
    .agent_name("repair_gather")
    .vector(sctx.vector().clone())
    .thread_store(ThreadStore::new(
        sctx.storage().clone(),
        sctx.scope().clone(),
        sctx.keyspace().clone(),
    ))
    .resolved_config(sctx.resolved_config().clone())
    .build();

    let system_prompt = format!(
        "\
You are a dbt repair investigator. Read the relevant SQL models, YAML schema files, \
and query the vector store for similar past errors.\n\n\
STRICT RULES:\n\
- NEVER call the same tool with the same arguments twice.\n\
- Read each file ONCE. Do not re-read files you have already read.\n\
- If vect_query returns empty results, do NOT retry the same query.\n\
- You have a strict budget of {GATHER_MAX_STEPS} tool calls. Be efficient.\n\
- When you have read the failing model and its upstream refs, STOP."
    );

    let history = session_log.format_for_prompt();
    let question = format!(
        "Investigate this dbt failure.\n\n{history}\n\n\
         [Repair iteration {iter} of {max}]\n\n\
         Follow these steps in order:\n\
         1. Read the failing model SQL file.\n\
         2. Read the YAML schema file for the failing model (e.g. models/staging/<model>.yml). \
            If the error message mentions a specific YAML file, read that file.\n\
         3. Read each upstream model SQL file referenced via ref().\n\
         4. Optionally call sql_schema on upstream models for column types.\n\
         5. Call vect_query ONCE with scope \"doc\" to check for similar past repairs.\n\
         6. Finish immediately — do not repeat any reads.",
        iter = iteration + 1,
        max = DEFAULT_MAX_ITERATIONS,
    );

    let options = dispatch.task_call_options("repair_gather");

    // We intentionally ignore the outcome variant — the value is in the
    // tool observations left in the thread store, not in the completion payload.
    let _ = Agent::run_until_block_non_interactive(
        &registry,
        &actx,
        &system_prompt,
        &tools_card,
        &question,
        options,
    )
    .await;

    // ── Phase B: structured diagnosis extraction ─────────────────────────
    let investigation = collect_investigation_results(sctx, &gather_tid).await;
    let error_brief = session_log.format_for_prompt();

    let diag = extract_diagnosis_structured(
        sctx,
        dispatch,
        &error_brief,
        &investigation.context_prompt,
        iteration,
    )
    .await?;

    Ok(GatheredContext {
        files: investigation.files,
        diagnosis: diag.diagnosis,
        upstream_schemas: diag.upstream_schemas,
        original_errors_resolved: diag.original_errors_resolved,
    })
}

/// Structured output from parsing the gather agent's thread.
struct InvestigationResults {
    /// Prompt-ready string of all tool observations (for diagnosis LLM).
    context_prompt: String,
    /// Structured file reads extracted from `file(op:"get")` observations.
    files: Vec<GatheredFile>,
}

/// Parse the gather agent's thread into both a prompt string (for the diagnosis
/// LLM) and structured file contents (piped through to reason & history).
///
/// Deduplicates by (tool_name, args) so repeated calls don't bloat context.
async fn collect_investigation_results(sctx: &SuiteCtx, thread_id: &str) -> InvestigationResults {
    let store = ThreadStore::new(
        sctx.storage().clone(),
        sctx.scope().clone(),
        sctx.keyspace().clone(),
    );
    let log = match store.get(thread_id).await {
        Ok(log) => log,
        Err(_) => {
            return InvestigationResults {
                context_prompt: String::new(),
                files: Vec::new(),
            };
        }
    };

    let mut sections: Vec<String> = Vec::new();
    let mut files: Vec<GatheredFile> = Vec::new();
    let mut seen_files: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for step in &log.steps {
        if let react_core::session::ThreadStep::ToolEnd {
            name,
            args,
            observation,
            ..
        } = step
        {
            let args_brief = serde_json::to_string(args).unwrap_or_default();

            let dedup_key = format!("{name}:{args_brief}");
            if !seen.insert(dedup_key) {
                continue;
            }

            let obs_text = if observation.ok {
                serde_json::to_string(&observation.extra).unwrap_or_default()
            } else {
                let errs = observation.errors.join("; ");
                format!("ERROR: {errs}")
            };
            let obs_capped = if obs_text.len() > 3000 {
                format!("{}…(truncated)", &obs_text[..3000])
            } else {
                obs_text.clone()
            };
            sections.push(format!(
                "### Tool: {name}\nArgs: {args_brief}\n{obs_capped}"
            ));

            if name == "file" && observation.ok {
                let op = args.get("op").and_then(|v| v.as_str()).unwrap_or("");
                let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
                if op == "get" && !path.is_empty() && seen_files.insert(path.to_string()) {
                    let content = observation
                        .extra
                        .get("content")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&obs_text);
                    files.push(GatheredFile {
                        path: path.to_string(),
                        content: content.to_string(),
                    });
                }
            }
        }
    }
    InvestigationResults {
        context_prompt: sections.join("\n\n"),
        files,
    }
}

/// Single structured LLM call that produces a [`GatherDiagnosisV1`].
///
/// Uses `LlmExpectedFormat::JsonSchema` so the provider enforces the schema
/// server-side when available (GPT structured outputs).
async fn extract_diagnosis_structured(
    sctx: &SuiteCtx,
    dispatch: &ModelDispatch,
    error_brief: &str,
    investigation_context: &str,
    _iteration: usize,
) -> Result<GatherDiagnosisV1, String> {
    let schema = react_core::schema_registry::OpenAiStrictSchema::for_type::<GatherDiagnosisV1>(
        "repair.gather_diagnosis",
    )
    .map_err(|e| format!("schema build error: {e}"))?;

    let options = react_core::llm::LlmCallOptions {
        prompt_id: "repair.gather_diagnosis",
        model: Some(dispatch.reason_model.clone()),
        expected_format: react_core::llm::LlmExpectedFormat::JsonSchema(schema),
        reasoning_effort: Some(crate::env_util::repair_reasoning_effort()),
        ..Default::default()
    };

    let messages = vec![
        ChatMessage {
            role: ChatRole::System,
            content: "You are a dbt repair diagnostician. Analyse the error context and \
                      investigation results below and produce a structured JSON diagnosis.\n\n\
                      IMPORTANT: For upstream_schemas, extract the EXACT column names from the \
                      SQL model files in the investigation results. Look at each upstream model's \
                      final SELECT statement and list every column name it produces. \
                      Do NOT guess or rename — use the exact names from the SQL."
                .to_string(),
        },
        ChatMessage {
            role: ChatRole::User,
            content: format!(
                "## Error Context\n{error_brief}\n\n\
                 ## Investigation Results\n{investigation_context}\n\n\
                 Produce a JSON object with:\n\
                 - \"diagnosis\": a thorough root-cause analysis\n\
                 - \"affected_files\": list of relative file paths that likely need fixing. \
                   Include BOTH .sql and .yml files when the issue involves column mismatches \
                   between SQL outputs and YAML schema declarations.\n\
                 - \"upstream_schemas\": for each upstream model referenced via ref() by the \
                   failing model(s), list the model_path and the exact column names from its \
                   final SELECT statement\n\
                 - \"original_errors_resolved\": set to true ONLY when ALL of these hold: \
                   (1) there are Prior Repair Attempts in the history above, \
                   (2) the most recent validation outcome shows the ORIGINAL errors from the \
                   Error Context are gone, and \
                   (3) any remaining failures are NEW errors not present in the original Error \
                   Context (e.g. new errors revealed since fixing the original errors). \
                   Set to false if this is the first iteration or any original error persists.",
            ),
        },
    ];

    let actx = build_tool_ctx(sctx, "repair_diagnosis");
    actx.llm_chat_json::<GatherDiagnosisV1>(&messages, &options)
        .await
        .map_err(|e| format!("diagnosis extraction failed: {e}"))
}

/// Wrapper struct so the LLM returns a top-level JSON object (required by
/// OpenAI Structured Outputs) instead of a bare array.
#[derive(Clone, Debug, serde::Serialize, Deserialize, schemars::JsonSchema)]
struct RepairFixPlanV1 {
    /// One or more fixes to apply. Each fix targets a single file.
    fixes: Vec<PlannedFix>,
}

/// Stage 2: Reason — single LLM call with the reason_model, no tools.
/// Produces a JSON object containing an array of planned fixes, enforced
/// via `LlmExpectedFormat::JsonSchema`.
async fn run_reason(
    sctx: &SuiteCtx,
    dispatch: &ModelDispatch,
    gathered: &GatheredContext,
    session_log: &RepairSessionLog,
    _iteration: usize,
) -> Result<Vec<PlannedFix>, String> {
    let history = session_log.format_for_prompt();
    let dialect = resolved_config_from_ctx_sctx(sctx)
        .map(|cfg| crate::dialect::active_provider_dialect(cfg))
        .unwrap_or_else(|| "Unknown SQL dialect".into());

    let schema = react_core::schema_registry::OpenAiStrictSchema::for_type::<RepairFixPlanV1>(
        "repair.fix_plan",
    )
    .map_err(|e| format!("schema build error: {e}"))?;

    let upstream_block = if !gathered.upstream_schemas.is_empty() {
        let mut lines = vec![
            "## Upstream Model Contracts (SOURCE OF TRUTH)".to_string(),
            "These are the ONLY columns available from each upstream model. \
             Do NOT reference any column not listed here."
                .to_string(),
        ];
        for schema in &gathered.upstream_schemas {
            lines.push(format!("\n### {}", schema.model_path));
            lines.push(format!("Columns: {}", schema.columns.join(", ")));
        }
        lines.join("\n")
    } else {
        String::new()
    };

    let files_block = if !gathered.files.is_empty() {
        let mut lines = vec![
            "## Current File Contents".to_string(),
            "These are the EXACT current contents of the files you may need to fix. \
             When using op \"write\", base your output on this content — do NOT guess the file structure."
                .to_string(),
        ];
        for gf in &gathered.files {
            let lang = if gf.path.ends_with(".yml") || gf.path.ends_with(".yaml") {
                "yaml"
            } else {
                "sql"
            };
            lines.push(format!(
                "\n### `{}`\n```{lang}\n{}\n```",
                gf.path, gf.content
            ));
        }
        lines.join("\n")
    } else {
        String::new()
    };

    let prompt = format!(
        "You are a senior dbt engineer. Based on the diagnosis, current file contents, and \
         full repair history below, produce fixes for the failing models.\n\n\
         SQL dialect: {dialect}\n\n\
         ## Diagnosis\n{diagnosis}\n\n\
         {upstream_block}\n\n\
         {files_block}\n\n\
         ## Full Repair History\n{history}\n\n\
         Rules:\n\
         - If a prior patch attempt failed, use op \"write\" instead of \"patch\".\n\
         - Fix BOTH SQL and YAML files as needed. When columns are added to or removed from \
           a SQL model's final SELECT, the corresponding YAML schema file (e.g. \
           models/staging/<model>.yml) MUST be updated to match. SQL and YAML are a contract pair.\n\
         - Produce at least one fix.\n\
         - When using op \"write\", output the COMPLETE file content based on the Current File \
           Contents above. Do NOT invent SQL structure — modify the existing content.\n\
         - CRITICAL: Only SELECT columns that appear in the Upstream Model Contracts above. \
           If you need a different output name, use a SQL alias \
           (e.g. `created_at_ts as customer_created_at_ts`). \
           NEVER reference a column that does not exist upstream.",
        diagnosis = gathered.diagnosis,
    );

    let messages = vec![
        ChatMessage {
            role: ChatRole::System,
            content: "You are a dbt repair planner.".to_string(),
        },
        ChatMessage {
            role: ChatRole::User,
            content: prompt,
        },
    ];

    let options = react_core::llm::LlmCallOptions {
        prompt_id: "repair.fix_plan",
        model: Some(dispatch.reason_model.clone()),
        expected_format: react_core::llm::LlmExpectedFormat::JsonSchema(schema),
        reasoning_effort: Some(crate::env_util::repair_reasoning_effort()),
        ..Default::default()
    };

    let actx = build_tool_ctx(sctx, "repair_reason");
    let plan: RepairFixPlanV1 = actx
        .llm_chat_json(&messages, &options)
        .await
        .map_err(|e| format!("reason stage LLM call failed: {e}"))?;

    Ok(plan.fixes)
}

/// Stage 3: Apply — mechanically apply each planned fix.
async fn apply_fixes(sctx: &SuiteCtx, thread_id: &str, fixes: &[PlannedFix]) -> Vec<ApplyResult> {
    let actx = build_tool_ctx(sctx, thread_id);
    let datasets = crate::ctx_ext::sctx_datasets(sctx);
    let mut results = Vec::new();

    for fix in fixes {
        let result = match fix.op {
            FileOp::Patch => {
                let base_state =
                    crate::patch_protocol::read_patch_base_state(&actx, &fix.path).await;
                match crate::patch_protocol::apply_single_file_patch_with_base(
                    &actx,
                    datasets
                        .as_ref()
                        .map(|a| a as &Arc<dyn crate::providers::DatasetCatalogProvider>),
                    &fix.path,
                    &fix.content,
                    &base_state,
                )
                .await
                {
                    Ok(outcome) => {
                        let _ = actx
                            .storage()
                            .put_bytes(&outcome.key, outcome.content.as_bytes(), "text/plain")
                            .await;
                        ApplyResult {
                            path: fix.path.clone(),
                            op: FileOp::Patch,
                            success: true,
                            error: None,
                        }
                    }
                    Err(e) => ApplyResult {
                        path: fix.path.clone(),
                        op: FileOp::Patch,
                        success: false,
                        error: Some(e),
                    },
                }
            }
            FileOp::Write => {
                match crate::project_fs::write_file(
                    &actx,
                    datasets
                        .as_ref()
                        .map(|a| a as &Arc<dyn crate::providers::DatasetCatalogProvider>),
                    &fix.path,
                    &fix.content,
                )
                .await
                {
                    Ok(_) => ApplyResult {
                        path: fix.path.clone(),
                        op: FileOp::Write,
                        success: true,
                        error: None,
                    },
                    Err(e) => ApplyResult {
                        path: fix.path.clone(),
                        op: FileOp::Write,
                        success: false,
                        error: Some(e),
                    },
                }
            }
        };
        results.push(result);
    }

    results
}

fn build_gather_tools(sctx: &SuiteCtx) -> Result<ToolRegistry, String> {
    use crate::tools::{
        files_tool::FilesTool, sql_run::SqlRunTool, sql_sample::SqlSampleTool,
        sql_schema::SqlSchemaTool, sql_stats::SqlStatsTool, vect_query::VectQueryTool,
    };

    let query =
        crate::ctx_ext::sctx_query(sctx).ok_or_else(|| "query provider missing".to_string())?;

    let mut reg = ToolRegistry::new();
    reg.register(FilesTool {
        datasets: crate::ctx_ext::sctx_datasets(sctx),
    });
    reg.register(SqlSchemaTool {
        query: query.clone(),
        datasets: crate::ctx_ext::sctx_datasets(sctx),
        catalog: crate::ctx_ext::sctx_catalog(sctx),
    });
    reg.register(SqlStatsTool {
        catalog: crate::ctx_ext::sctx_catalog(sctx),
        datasets: crate::ctx_ext::sctx_datasets(sctx),
    });
    reg.register(SqlSampleTool {
        query: query.clone(),
    });
    reg.register(SqlRunTool {
        query: query.clone(),
    });
    reg.register(VectQueryTool);
    Ok(reg)
}

fn gather_tools_card() -> String {
    [
        "Allowed tools (repair gather phase, read-only investigation):",
        "- file(args:{op:\"list\"|\"get\", prefix?:string, path?:string, limit?:int, max_chars?:int})",
        "- sql_schema(args:{table?:string})",
        "- sql_stats(args:{table:string, field:string})",
        "- sql_sample(args:{table:string, field:string, k:int})",
        "- run_sql(args:{sql:string})",
        "- vect_query(args:{scope:\"dataset\"|\"field\"|\"doc\"|\"artifact\"|\"metric\"|\"model\", query_text:string, k:int})",
        "",
        "Not available: file mutations (patch/write/rm/mv), dbt_validate, staging_model, gold_model.",
    ]
    .join("\n")
}

fn build_tool_ctx(sctx: &SuiteCtx, thread_id: &str) -> AgentCtx {
    let thread_store = ThreadStore::new(
        sctx.storage().clone(),
        sctx.scope().clone(),
        sctx.keyspace().clone(),
    );
    let mut actx = AgentCtxBuilder::new(
        sctx.llm().clone(),
        sctx.storage().clone(),
        sctx.scope().clone(),
        sctx.keyspace().clone(),
        Arc::new(react_core::agent::DefaultPolicy),
    )
    .top_k(crate::env_util::DEFAULT_TOP_K)
    .per_step_timeout_secs(10)
    .max_steps(GATHER_MAX_STEPS)
    .thread_id(thread_id)
    .trace_tx(sctx.trace_tx().clone())
    .agent_name("repair")
    .vector(sctx.vector().clone())
    .thread_store(thread_store)
    .resolved_config(sctx.resolved_config().clone())
    .build();
    crate::ctx_ext::copy_capabilities_to_actx(sctx, &mut actx);
    actx
}

/// All context gathered in Stage 1.  Every field is mandatory — there is no
/// `Default` impl so the compiler forces you to populate every field.
struct GatheredContext {
    /// SQL/YAML files read by the gather agent, with their full content.
    files: Vec<GatheredFile>,
    /// Root-cause analysis from the diagnosis LLM call.
    diagnosis: String,
    /// Column schemas extracted from upstream models.
    upstream_schemas: Vec<UpstreamModelSchema>,
    /// LLM assessment: original errors resolved but new cascade errors appeared.
    original_errors_resolved: bool,
}

/// Index a repair iteration to the vector store for future retrieval.
async fn index_to_vector_store(sctx: &SuiteCtx, entry: &RepairIteration, success: bool) {
    let vector = match sctx.vector().as_ref() {
        Some(v) => v,
        None => return,
    };

    let text = format!(
        "Repair attempt ({}): {}\nFiles: {:?}\nDiagnosis: {}",
        if success { "success" } else { "failure" },
        entry.error_brief(),
        entry.files_changed(),
        entry.diagnosis,
    );

    let llm = sctx.llm();
    let embedding = match llm.embed(&[text.clone()]) {
        Ok(mut vecs) if !vecs.is_empty() => vecs.remove(0),
        _ => return,
    };

    let doc = crate::vector_docs::RepairMemoryDocument::new(
        format!("repair-{}", uuid::Uuid::new_v4()),
        text,
        embedding,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        crate::vector_docs::RepairMemoryMetadata {
            outcome: if success { "success" } else { "failure" }.to_string(),
            files_changed: entry.files_changed(),
        },
    );

    let _ =
        react_core::provider_traits::upsert_typed_documents(vector.as_ref(), sctx.scope(), &[doc])
            .await;
}

fn resolved_config_from_ctx_sctx(
    sctx: &SuiteCtx,
) -> Option<&react_core::resolved_config::ReactResolvedConfig> {
    sctx.resolved_config().as_ref().map(|c| c.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repair_fix_plan_v1_round_trips() {
        let json =
            r#"{"fixes":[{"path":"models/stg_orders.sql","op":"write","content":"SELECT 1"}]}"#;
        let plan: RepairFixPlanV1 = serde_json::from_str(json).unwrap();
        assert_eq!(plan.fixes.len(), 1);
        assert_eq!(plan.fixes[0].path, "models/stg_orders.sql");
        assert_eq!(plan.fixes[0].op, FileOp::Write);
    }

    #[test]
    fn repair_fix_plan_v1_schema_is_valid() {
        let schema = react_core::schema_registry::OpenAiStrictSchema::for_type::<RepairFixPlanV1>(
            "test.repair_fix_plan",
        );
        assert!(schema.is_ok(), "schema generation must succeed");
    }

    #[test]
    fn gather_diagnosis_v1_schema_is_valid() {
        let schema = react_core::schema_registry::OpenAiStrictSchema::for_type::<GatherDiagnosisV1>(
            "test.gather_diagnosis",
        );
        assert!(schema.is_ok(), "schema generation must succeed");
    }
}
