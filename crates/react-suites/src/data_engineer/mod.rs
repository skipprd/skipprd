use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use react_core::agent::{Agent, AgentCtx, AgentPolicy, Interrupt, RunOutcome};
use crate::flow_frame::FlowFrame;
use react_core::session::ThreadStore;
use crate::preflight::PreflightProvider;
use crate::data_engineer_shared::policy_sql_validated::SqlValidatedPolicy;
use crate::data_engineer_shared::types::DatasetCandidate;
use crate::suite::{Suite, SuiteCtx};
use react_core::tools::{Tool, ToolRegistry};
use std::collections::HashMap;
use std::sync::Arc;

pub struct DataEngineerSuite;

pub mod prompts;
pub mod tools;
pub mod dbt_error;
pub mod control_flow;
pub mod project_fs;
pub mod dbt_repair;

/// Agent-mode policy: preserve strict interrupts (ask_user/ask_approval), but otherwise accept finals.
struct InterruptOnlyPolicy;

#[async_trait::async_trait]
impl AgentPolicy for InterruptOnlyPolicy {
    fn interrupt_for_action(&self, action_name: &str, args: &serde_json::Value, obs: &serde_json::Value) -> Option<Interrupt> {
        if action_name == "ask_user" {
            let prompt = args
                .get("prompt")
                .and_then(|x| x.as_str())
                .or_else(|| obs.get("prompt").and_then(|x| x.as_str()))
                .unwrap_or("Please provide additional context.")
                .to_string();
            return Some(Interrupt::AwaitUser { prompt });
        }
        if action_name == "ask_approval" {
            let prompt = args
                .get("prompt")
                .and_then(|x| x.as_str())
                .or_else(|| obs.get("prompt").and_then(|x| x.as_str()))
                .unwrap_or("Please review and approve/reject.")
                .to_string();
            return Some(Interrupt::AwaitApproval { prompt });
        }
        None
    }

    fn timeout_for_tool(&self, action_name: &str) -> Option<u64> {
        // Keep base tool timeouts snappy, but raise for known-slow operations.
        match action_name {
            // Can involve an inner LLM call + writes.
            "staging_model" => Some(60),
            // Warehouse queries can legitimately take >10s.
            "run_sql" => Some(60),
            // Batch writes can be larger.
            "approve_and_save_artifact_batch" => Some(60),
            // Storage reads/writes sometimes hit network latency.
            "dbt_files" => Some(30),
            _ => None,
        }
    }

    async fn handle_final(
        &self,
        tools: &ToolRegistry,
        ctx: &AgentCtx,
        transcript: &mut Vec<String>,
        store: Option<&react_core::session::ThreadStore>,
        thread_id: &str,
        final_obj: &serde_json::Value,
    ) -> Result<Option<RunOutcome>, String> {
        react_core::agent::DefaultPolicy
            .handle_final(tools, ctx, transcript, store, thread_id, final_obj)
            .await
    }
}

#[derive(Clone, Debug, Deserialize)]
struct ReviewMeta {
    #[serde(default)]
    actionable: bool,
    #[serde(default)]
    dataset_ids: Vec<String>,
    #[serde(default)]
    tier: String, // "silver"|"gold"|"unknown"
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AuthoringKind {
    Cleanse,
    Model,
}

impl DataEngineerSuite {
    fn phase_start_idx(log: &react_core::session::ThreadLog, phase: control_flow::Phase) -> Option<usize> {
        for (i, step) in log.steps.iter().enumerate().rev() {
            if step.action != "phase" {
                continue;
            }
            let p = step
                .args
                .get("phase")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if p == phase.as_str() {
                return Some(i);
            }
        }
        None
    }

    /// Agent-mode hardening: allow at most one `ask_approval` per authoring phase.
    ///
    /// Rationale: once the user has approved the table set/plan for the phase, repeated approval prompts
    /// can trap the authoring loop. In agent mode we want the model to proceed to scaffolding.
    fn allow_ask_approval_in_phase(log: Option<&react_core::session::ThreadLog>, phase: control_flow::Phase) -> bool {
        let Some(log) = log else { return true };
        let Some(start) = Self::phase_start_idx(log, phase) else { return true };
        // Monotonic: once approval/reject is seen within this phase, never allow ask_approval again
        // (even if later user messages are "continue", "ok", etc).
        for step in log.steps.iter().skip(start + 1) {
            if step.action != "user" {
                continue;
            }
            let t = step
                .args
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_lowercase();
            if t == "approve" || t == "reject" {
                return false;
            }
        }
        true
    }
    fn parse_review_meta(answer: &str) -> Option<ReviewMeta> {
        let first = answer.lines().next()?.trim();
        let prefix = "META:";
        if !first.starts_with(prefix) {
            return None;
        }
        let json_text = first[prefix.len()..].trim();
        serde_json::from_str::<ReviewMeta>(json_text).ok()
    }

    fn strip_meta_line(answer: &str) -> String {
        let mut lines = answer.lines();
        let first = lines.next().unwrap_or("").trim();
        if first.starts_with("META:") {
            lines.collect::<Vec<&str>>().join("\n").trim().to_string()
        } else {
            answer.trim().to_string()
        }
    }

    fn is_mutation_step_for_review(step: &react_core::session::ThreadStep) -> bool {
        match step.action.as_str() {
            "approve_and_save_artifact" | "approve_and_save_artifact_batch" | "staging_model" => true,
            "dbt_files" => step
                .args
                .get("op")
                .and_then(|v| v.as_str())
                .map(|op| op == "put")
                .unwrap_or(false),
            _ => false,
        }
    }

    fn compact_mutation_summary(step: &react_core::session::ThreadStep) -> serde_json::Value {
        let ok = step
            .observation
            .get("ok")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let action = step.action.clone();

        let args = match step.action.as_str() {
            "dbt_files" => serde_json::json!({
                "op": step.args.get("op"),
                "path": step.args.get("path"),
            }),
            "approve_and_save_artifact" => serde_json::json!({
                "kind": step.args.get("kind"),
                "name": step.args.get("name"),
                "dataset_id": step.args.get("dataset_id"),
            }),
            "approve_and_save_artifact_batch" => {
                let names: Vec<serde_json::Value> = step
                    .args
                    .get("items")
                    .and_then(|v| v.as_array())
                    .map(|items| {
                        items
                            .iter()
                            .take(6)
                            .filter_map(|it| it.get("name").cloned())
                            .collect()
                    })
                    .unwrap_or_default();
                serde_json::json!({
                    "items_count": step.args.get("items").and_then(|v| v.as_array()).map(|a| a.len()),
                    "names_head": names,
                })
            }
            "staging_model" => serde_json::json!({
                "dataset_ids": step.args.get("dataset_ids"),
                "written_keys": step.observation.get("written_keys"),
            }),
            _ => step.args.clone(),
        };

        serde_json::json!({
            "action": action,
            "ok": ok,
            "args": args,
        })
    }

    fn build_review_question_with_context(
        question: &str,
        phase: control_flow::Phase,
        log: Option<&react_core::session::ThreadLog>,
    ) -> String {
        let mut base = match phase {
            control_flow::Phase::CleanseReview => format!(
                "Review the DBT project after cleanse/staging work. Identify any issues or improvements to apply.\n\nOriginal goal:\n{}",
                question
            ),
            control_flow::Phase::ModelReview => format!(
                "Review the DBT project after modeling (core/gold) work. Identify any issues or improvements to apply.\n\nOriginal goal:\n{}",
                question
            ),
            _ => format!(
                "Final review after publish. Identify any remaining actionable improvements.\n\nOriginal goal:\n{}",
                question
            ),
        };

        let Some(log) = log else { return base };

        // Current entry reason (last phase step is authoritative for why we are in this phase).
        let entry = log.steps.iter().rev().find(|s| s.action == "phase");
        let entry_reason_code = entry.and_then(|s| s.args.get("reason_code")).and_then(|v| v.as_str());
        let entry_reason_detail = entry.and_then(|s| s.args.get("reason_detail")).cloned().unwrap_or(serde_json::Value::Null);

        // Prior review context: last phase transition emitted from a review decision.
        let mut prior_review_block: Option<String> = None;
        let mut mutations_since: Vec<serde_json::Value> = Vec::new();

        if let Some((idx, step)) = log.steps.iter().enumerate().rev().find(|(_, s)| {
            if s.action != "phase" {
                return false;
            }
            matches!(
                s.args.get("reason_code").and_then(|v| v.as_str()),
                Some("review_actionable_true") | Some("review_actionable_false")
            )
        }) {
            let rd = step.args.get("reason_detail").cloned().unwrap_or(serde_json::Value::Null);
            let review_phase = rd.get("review_phase").and_then(|v| v.as_str()).unwrap_or("unknown");
            let meta = rd.get("meta").cloned().unwrap_or(serde_json::Value::Null);
            let ans = rd.get("answer").and_then(|v| v.as_str()).unwrap_or("");
            let excerpt = {
                let cleaned = Self::strip_meta_line(ans);
                let max = 700usize;
                if cleaned.len() > max {
                    format!("{}...", &cleaned[..max])
                } else {
                    cleaned
                }
            };

            prior_review_block = Some(format!(
                "Previous review decision:\n- review_phase: {review_phase}\n- meta: {meta}\n- excerpt: {excerpt}",
                review_phase = review_phase,
                meta = meta,
                excerpt = excerpt.replace('\n', " "),
            ));

            // Mutations since that review decision.
            for step in log.steps.iter().skip(idx + 1) {
                if Self::is_mutation_step_for_review(step) {
                    mutations_since.push(Self::compact_mutation_summary(step));
                    if mutations_since.len() >= 12 {
                        break;
                    }
                }
            }
        }

        let mut ctx_lines: Vec<String> = Vec::new();
        if let Some(prior) = prior_review_block {
            ctx_lines.push(prior);
        }
        if !mutations_since.is_empty() {
            ctx_lines.push(format!(
                "What changed since previous review (mutations, newest-first not guaranteed):\n{}",
                mutations_since
                    .iter()
                    .map(|v| format!("- {}", v))
                    .collect::<Vec<String>>()
                    .join("\n")
            ));
        }
        if entry_reason_code.is_some() || !entry_reason_detail.is_null() {
            ctx_lines.push(format!(
                "Why we are reviewing now:\n- entry_reason_code: {}\n- entry_reason_detail: {}",
                entry_reason_code.unwrap_or("null"),
                entry_reason_detail
            ));
        }

        if !ctx_lines.is_empty() {
            base = format!(
                "Review context (from thread history):\n{}\n\n{}",
                ctx_lines.join("\n\n"),
                base
            );
        }

        base
    }

    fn validate_agent_type(agent_type: &str) -> Result<(), String> {
        match agent_type {
            "ask" | "model" | "cleanse" | "review" | "agent" => Ok(()),
            _ => Err(format!(
                "invalid agent_type '{}' for suite 'data_engineer' (expected 'ask' | 'model' | 'cleanse' | 'review' | 'agent')",
                agent_type
            )),
        }
    }

    fn inject_review_question(question: &str) -> String {
        format!(
            "Review request: {}.\n\
             Act as a read-only, practical reviewer for the current DBT project.\n\
             - Stay read-only (no edits/publish).\n\
             - Use artifacts and schema tools to ground feedback.\n\
             - Prioritize business value over academic correctness; avoid pedantic nitpicks.\n\
             - Recommend changes/tests only when they materially improve correctness, reduce business risk, or improve analyst usability.\n\
             - Provide prioritized, dataset-scoped improvements (few, high-impact).\n\
             - IMPORTANT: Re-check the CURRENT project state (prefer target/manifest.json + models/schema.yml). If your feedback is substantially unchanged from the prior iteration, set META.actionable=false (do not repeat the same advice).",
            question
        )
    }

    fn inject_model_question(question: &str) -> String {
        format!(
            "Modeling goal: {}.\n\
             Act as a proactive DBT Engineer with strong business domain focus.\n\
             - Do NOT assume table names.\n\
             - If embeddings/vect search yields no candidates, call sql_schema with no args to list tables.\n\
             - Hybrid selection: propose a shortlist of tables (with brief reasons based on schema/stats), then ask_approval to confirm the table list before writing any artifacts.\n\
             - Tiers:\n\
               - Silver = DBT staging models (cleansed/normalized) written by `staging_model` and materialized into the configured Athena silver database.\n\
               - Gold = DBT marts/final models materialized into the configured Athena gold database.\n\
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
             - IMPORTANT: use `dbt_files op=patch` for real DBT project files (e.g. path='dbt_project.yml', 'packages.yml', 'models/schema.yml', and staging SQL like 'models/staging/stg_<table>.sql' (or 'models/staging/stg_<schema>__<table>.sql' to avoid collisions), 'models/core/...'). Use approve_and_save_artifact(_batch) only for models/metrics.\n\
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
             - Silver tier must land in the configured Athena silver database.\n\
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
             - Use `dbt_files op=patch` for dbt_project.yml, packages.yml, and YAML (especially models/schema.yml). For staging SQL, prefer 'models/staging/stg_<table>.sql' but use 'models/staging/stg_<schema>__<table>.sql' to avoid collisions.\n\
             - After saving artifacts: ALWAYS validate with dbt_validate. If validate fails, iterate (edit artifacts, re-validate) until clean.\n\
             - When validation is clean: call publish_dbt_to_provider (views by default; propose tables/incremental with rationale and await approval).\n\
             - Use catalog_note to record notable cleansing decisions and assumptions (preview if material).",
            question
        )
    }

    fn build_tools(agent_type: &str, sctx: &SuiteCtx) -> Result<ToolRegistry, String> {
        use crate::data_engineer::tools::{
            artifacts::ArtifactsTool,
            dbt_files::DbtFilesTool,
            sql_run::SqlRunTool,
            sql_sample::SqlSampleTool,
            sql_schema::SqlSchemaTool,
            sql_stats::SqlStatsTool,
            vect_query::VectQueryTool,
        };

        /// Thread-derived guard: blocks repeated dbt_validate after failure until a mutation occurs,
        /// and enforces a data probe after runtime (build/run) failures.
        struct ThreadDerivedDbtValidateTool {
            inner: tools::dbt_validate::DbtValidateTool,
        }
        #[async_trait::async_trait]
        impl react_core::tools::Tool for ThreadDerivedDbtValidateTool {
            fn name(&self) -> &'static str { "dbt_validate" }
            async fn call(&self, args: serde_json::Value, ctx: &react_core::agent::AgentCtx) -> Result<serde_json::Value, String> {
                let build = args.get("build").and_then(|v| v.as_bool()).unwrap_or(false);
                let run = args.get("run").and_then(|v| v.as_bool()).unwrap_or(false);
                let runtime_validate = build || run;
                if let (Some(store), Some(tid)) = (ctx.thread_store.as_ref(), ctx.thread_id.as_deref()) {
                    let log = store.get(tid).await;
                    let guard = crate::data_engineer::control_flow::derive_guard_state(log.as_ref());
                    if guard.last_validate_failed && !guard.mutated_since_fail {
                        return Err(
                            "dbt_validate is blocked after a failed validation until you APPLY A FIX to the dbt project.\n\
                             Next step must be a mutating fix action (e.g. `staging_model` or `dbt_files op=patch` or approve_and_save_artifact_batch to update schema/tests)."
                                .to_string(),
                        );
                    }
                    if runtime_validate && guard.probe_required && !guard.probe_satisfied {
                        return Err(
                            "dbt_validate (build/run) is blocked after a runtime failure until you run meaningful SQL probes.\n\
                             Next step must include `run_sql` against the failing relation(s) (not `SELECT 1`) to diagnose data issues, then APPLY a fix."
                                .to_string(),
                        );
                    }
                }
                self.inner.call(args, ctx).await
            }
        }

        let mut registry = ToolRegistry::new();

        let query = sctx
            .query
            .as_ref()
            .ok_or_else(|| "query provider missing".to_string())?
            .clone();

        // Shared analytics tools (note: run_sql is registered per-agent so authoring agents can be guarded)
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
            // review uses read-only tools only
            "review" => {
                // Allow review to read the current dbt project state (manifest/schema/models)
                // without permitting writes.
                struct ReadOnlyDbtFilesTool {
                    inner: DbtFilesTool,
                }
                #[async_trait::async_trait]
                impl react_core::tools::Tool for ReadOnlyDbtFilesTool {
                    fn name(&self) -> &'static str { "dbt_files" }
                    async fn call(&self, args: serde_json::Value, ctx: &react_core::agent::AgentCtx) -> Result<serde_json::Value, String> {
                        let op = args.get("op").and_then(|x| x.as_str()).unwrap_or("get");
                        if op == "patch" {
                            return Err("dbt_files is read-only for review; use op='get' or op='list'".to_string());
                        }
                        self.inner.call(args, ctx).await
                    }
                }
                registry.register(ReadOnlyDbtFilesTool { inner: DbtFilesTool { datasets: sctx.datasets.clone() } });
                registry.register(ArtifactsTool);
            }
            // cleanse uses shared tools + authoring/validation/publish loop
            "cleanse" => {
                registry.register(SqlRunTool { query: query.clone() });
                registry.register(tools::ask_user::AskUserTool);
                registry.register(tools::ask_approval::AskApprovalTool);
                registry.register(tools::approve_save::ApproveAndSaveArtifactTool);
                registry.register(tools::approve_save_batch::ApproveAndSaveArtifactBatchTool { datasets: sctx.datasets.clone() });
                registry.register(tools::dbt_examples::SearchDbtExamplesTool);
                registry.register(tools::staging_model::StagingModelTool { datasets: sctx.datasets.clone() });
                registry.register(ThreadDerivedDbtValidateTool { inner: tools::dbt_validate::DbtValidateTool { datasets: sctx.datasets.clone(), catalog: sctx.catalog.clone() } });
                registry.register(tools::publish_dbt_to_provider::PublishDbtToProviderTool { datasets: sctx.datasets.clone(), catalog: sctx.catalog.clone() });
                registry.register(tools::sql_register::SqlRegisterTool);
                registry.register(tools::catalog_note::CatalogNoteTool);
                registry.register(DbtFilesTool { datasets: sctx.datasets.clone() });
                registry.register(ArtifactsTool);
            }
            // ask uses shared tools + user/approval interrupts + artifacts
            "ask" => {
                registry.register(SqlRunTool { query: query.clone() });
                registry.register(tools::ask_user::AskUserTool);
                registry.register(tools::ask_approval::AskApprovalTool);
                registry.register(DbtFilesTool { datasets: sctx.datasets.clone() });
                registry.register(ArtifactsTool);
            }
            // model uses ask tools + artifact authoring + dbt helpers + artifacts
            _ => {
                registry.register(SqlRunTool { query: query.clone() });
                registry.register(tools::ask_user::AskUserTool);
                registry.register(tools::ask_approval::AskApprovalTool);
                registry.register(tools::approve_save::ApproveAndSaveArtifactTool);
                registry.register(tools::approve_save_batch::ApproveAndSaveArtifactBatchTool { datasets: sctx.datasets.clone() });
                registry.register(tools::dbt_examples::SearchDbtExamplesTool);
                registry.register(tools::staging_model::StagingModelTool { datasets: sctx.datasets.clone() });
                registry.register(ThreadDerivedDbtValidateTool { inner: tools::dbt_validate::DbtValidateTool { datasets: sctx.datasets.clone(), catalog: sctx.catalog.clone() } });
                registry.register(tools::publish_dbt_to_provider::PublishDbtToProviderTool { datasets: sctx.datasets.clone(), catalog: sctx.catalog.clone() });
                registry.register(tools::sql_register::SqlRegisterTool);
                registry.register(tools::catalog_note::CatalogNoteTool);
                registry.register(DbtFilesTool { datasets: sctx.datasets.clone() });
                registry.register(ArtifactsTool);
            }
        }

        Ok(registry)
    }

    fn build_tools_for_phase(
        phase: control_flow::Phase,
        guard: &control_flow::DerivedGuardState,
        allow_ask_approval: bool,
        sctx: &SuiteCtx,
    ) -> Result<(ToolRegistry, String), String> {
        use crate::data_engineer::tools::{
            artifacts::ArtifactsTool,
            dbt_files::DbtFilesTool,
            sql_run::SqlRunTool,
            sql_sample::SqlSampleTool,
            sql_schema::SqlSchemaTool,
            sql_stats::SqlStatsTool,
            vect_query::VectQueryTool,
        };

        let query = sctx
            .query
            .as_ref()
            .ok_or_else(|| "query provider missing".to_string())?
            .clone();

        let mut reg = ToolRegistry::new();

        // Common read tools (safe in most phases)
        reg.register(SqlSchemaTool {
            query: query.clone(),
            datasets: sctx.datasets.clone(),
            catalog: sctx.catalog.clone(),
        });
        reg.register(SqlStatsTool {
            catalog: sctx.catalog.clone(),
            datasets: sctx.datasets.clone(),
        });
        reg.register(SqlSampleTool { query: query.clone() });
        reg.register(VectQueryTool);
        reg.register(ArtifactsTool);

        let mut tools_card_lines: Vec<&'static str> = Vec::new();

        match phase {
            control_flow::Phase::CleanseAuthor | control_flow::Phase::ModelAuthor => {
                // Authoring phases: allow investigation + mutations; validation/publish are suite-driven.
                //
                // If the last validation failed and no mutation has happened since, enforce a hard tool lock:
                // the next step MUST be a mutation.
                let hard_mutation_only = guard.last_validate_failed && !guard.mutated_since_fail;

                reg.register(tools::ask_user::AskUserTool);
                if allow_ask_approval {
                    reg.register(tools::ask_approval::AskApprovalTool);
                }
                reg.register(tools::approve_save::ApproveAndSaveArtifactTool);
                reg.register(tools::approve_save_batch::ApproveAndSaveArtifactBatchTool { datasets: sctx.datasets.clone() });

                if hard_mutation_only {
                    // Patch-only dbt_files to avoid "read-only thrash" when we require a mutation next.
                    struct PutOnlyDbtFilesTool {
                        inner: DbtFilesTool,
                    }
                    #[async_trait::async_trait]
                    impl react_core::tools::Tool for PutOnlyDbtFilesTool {
                        fn name(&self) -> &'static str { "dbt_files" }
                        async fn call(&self, args: serde_json::Value, ctx: &react_core::agent::AgentCtx) -> Result<serde_json::Value, String> {
                            let op = args.get("op").and_then(|x| x.as_str()).unwrap_or("get");
                            if op != "patch" {
                                return Err("dbt_files is patch-only right now (a mutating fix is required before any further validation).".to_string());
                            }
                            self.inner.call(args, ctx).await
                        }
                    }
                    reg.register(PutOnlyDbtFilesTool { inner: DbtFilesTool { datasets: sctx.datasets.clone() } });

                    tools_card_lines = vec![
                        "Allowed tools (authoring phase; HARD constraint: mutation required next):",
                        "- approve_and_save_artifact(args:{kind,name,content,dataset_id?,preview_diff?})",
                        "- approve_and_save_artifact_batch(args:{items:[...],preview_diff?})",
                        "- dbt_files(args:{op:\"patch\",path:string,patch_text?:string,content?:string,base_sha256?:string,create_if_missing?:bool,preview_diff?:bool})",
                        "- ask_user(args:{prompt:string})",
                        "- ask_approval(args:{prompt:string}) (only if registered; otherwise forbidden in this phase)",
                        "",
                        "Not available: read/explore tools, dbt_validate, publish_dbt_to_provider.",
                    ];
                } else {
                    // Normal authoring: allow read/explore + probes.
                    reg.register(tools::staging_model::StagingModelTool { datasets: sctx.datasets.clone() });
                    reg.register(SqlRunTool { query: query.clone() });
                    reg.register(tools::dbt_examples::SearchDbtExamplesTool);
                    reg.register(DbtFilesTool { datasets: sctx.datasets.clone() });

                    tools_card_lines = vec![
                        "Allowed tools (authoring phase):",
                        "- sql_schema(args:{table?:string})",
                        "- vect_query(args:{scope:\"dataset\"|\"field\"|\"doc\"|\"artifact\"|\"metric\"|\"model\", query_text:string, k:int})",
                        "  - IMPORTANT: arg key is query_text (NOT query). scope must be one of the listed strings (NOT \"table\").",
                        "- sql_stats(args:{table:string, field:string}) (requires field; no table-only mode)",
                        "- sql_sample(args:{table:string, field:string, k:int}) (top values for a FIELD; not a row sampler)",
                        "- run_sql(args:{sql:string}) (use this to sample rows: SELECT * FROM <table> LIMIT 20)",
                        "- staging_model(args:{dataset_ids:[string], instructions?:string, sql?:string|staging_model?:string|expression?:string})",
                        "  - IMPORTANT: you MUST provide dataset_ids. This tool will NOT default to all datasets.",
                        "- approve_and_save_artifact(args:{kind,name,content,dataset_id?,preview_diff?})",
                        "- approve_and_save_artifact_batch(args:{items:[...],preview_diff?})",
                        "- dbt_files(args:{op:\"list\"|\"get\"|\"get_json\"|\"manifest_find\"|\"patch\", path?:string, prefix?:string, patch_text?:string, content?:string, base_sha256?:string, create_if_missing?:bool, pointer?:string, limit?:int, max_chars?:int, preview_diff?:bool})",
                        "- ask_user(args:{prompt:string})",
                        "- ask_approval(args:{prompt:string}) (only if registered; otherwise forbidden in this phase)",
                        "",
                        "Not available in this phase: dbt_validate, publish_dbt_to_provider (suite handles these deterministically).",
                    ];
                }
            }
            control_flow::Phase::CleanseReview
            | control_flow::Phase::ModelReview
            | control_flow::Phase::PostPublishReview => {
                // Review phases: keep read-only; do not allow arbitrary SQL execution.
                struct ReadOnlyDbtFilesTool {
                    inner: DbtFilesTool,
                }
                #[async_trait::async_trait]
                impl react_core::tools::Tool for ReadOnlyDbtFilesTool {
                    fn name(&self) -> &'static str { "dbt_files" }
                    async fn call(&self, args: serde_json::Value, ctx: &react_core::agent::AgentCtx) -> Result<serde_json::Value, String> {
                        let op = args.get("op").and_then(|x| x.as_str()).unwrap_or("get");
                        if op == "patch" {
                            return Err("dbt_files is read-only in review phases; use op='get' or op='list'".to_string());
                        }
                        self.inner.call(args, ctx).await
                    }
                }
                reg.register(ReadOnlyDbtFilesTool { inner: DbtFilesTool { datasets: sctx.datasets.clone() } });

                tools_card_lines = vec![
                    "Allowed tools (review phase, read-only):",
                    "- dbt_files (list/get/get_json/manifest_find)",
                    "- artifacts",
                    "- sql_schema / sql_stats / sql_sample / vect_query (read-only context)",
                    "",
                    "Not available: run_sql, staging_model, approve_and_save_artifact(_batch), dbt_validate, publish_dbt_to_provider.",
                ];
            }
            _ => {
                // Other phases do not run an LLM action set (suite does deterministic steps).
                tools_card_lines = vec!["Allowed tools: (suite deterministic step; no agent tools)"];
            }
        }

        Ok((reg, tools_card_lines.join("\n")))
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
                let empty: HashMap<String, react_core::discover::Metadata> = HashMap::new();
                if let Err(e) = cat
                    .build_all_with_progress(&sctx.scope, datasets.as_ref(), &empty, None)
                    .await
                {
                    tracing::warn!("data_engineer: catalog bootstrap failed: {}", e);
                }
            }
        }
    }

    async fn manifest_targeting_lines(
        actx: &AgentCtx,
        runtime_failures: &[serde_json::Value],
    ) -> Vec<String> {
        // Best-effort: map failing test(s) -> model file path(s) using target/manifest.json.
        let base = actx.keyspace.dbt_prefix(&actx.scope).trim_end_matches('/').to_string();
        let manifest_key = format!("{}/target/manifest.json", base);
        let bytes = match actx.storage.get_bytes(&manifest_key).await {
            Ok(b) => b,
            Err(_) => return vec![],
        };
        let v: serde_json::Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => return vec![],
        };
        let Some(nodes) = v.get("nodes").and_then(|n| n.as_object()) else {
            return vec![];
        };

        let mut out: Vec<String> = Vec::new();
        for rf in runtime_failures.iter().take(3) {
            let Some(name) = rf.get("name").and_then(|x| x.as_str()) else { continue };

            // Find the manifest node for this test.
            let mut test_node: Option<(&String, &serde_json::Value)> = None;
            for (k, node) in nodes.iter() {
                let rt = node.get("resource_type").and_then(|x| x.as_str()).unwrap_or("");
                if rt != "test" {
                    continue;
                }
                let n = node.get("name").and_then(|x| x.as_str()).unwrap_or("");
                if n == name || k.ends_with(name) {
                    test_node = Some((k, node));
                    break;
                }
            }
            let Some((_test_id, test_node)) = test_node else { continue };
            let test_file = test_node
                .get("original_file_path")
                .or_else(|| test_node.get("path"))
                .and_then(|x| x.as_str())
                .unwrap_or("");

            let depends = test_node
                .get("depends_on")
                .and_then(|d| d.get("nodes"))
                .and_then(|a| a.as_array())
                .cloned()
                .unwrap_or_default();
            let mut model_id: Option<String> = None;
            for d in depends {
                if let Some(s) = d.as_str() {
                    if s.starts_with("model.") {
                        model_id = Some(s.to_string());
                        break;
                    }
                }
            }
            let Some(mid) = model_id else { continue };
            let model_node = nodes.get(&mid);
            let model_file = model_node
                .and_then(|n| n.get("original_file_path").or_else(|| n.get("path")))
                .and_then(|x| x.as_str())
                .unwrap_or("");
            out.push(format!(
                "- {} -> model_file: {} ; test_file: {}",
                name,
                if model_file.is_empty() { "(unknown)" } else { model_file },
                if test_file.is_empty() { "(unknown)" } else { test_file }
            ));
        }
        out
    }

    async fn run_ask(thread_id: &str, question: &str, sctx: &SuiteCtx) -> Result<Vec<FlowFrame>, String> {
        let sys = crate::util::time_context::with_time_context(prompts::ask_system_prompt());
        let tools_card = prompts::ask_tool_card();

        let pf = crate::preflight::CatalogPreflightProvider {
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
            runtime: sctx
                .resolved_config
                .clone()
                .map(|c| c as Arc<dyn std::any::Any + Send + Sync>),
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

    async fn run_review(thread_id: &str, question: &str, sctx: &SuiteCtx) -> Result<Vec<FlowFrame>, String> {
        Self::ensure_catalog_bootstrap(sctx).await;
        let sys = crate::util::time_context::with_time_context(prompts::review_system_prompt());
        let tools_card = prompts::review_tool_card();

        let registry = Self::build_tools("review", sctx)?;
        let thread_store = ThreadStore::new(sctx.storage.clone(), sctx.scope.clone(), sctx.keyspace.clone());

        let actx = AgentCtx {
            top_k: 30,
            per_step_timeout_secs: 10,
            max_steps: 40,
            thread_id: Some(thread_id.to_string()),
            progress_tx: None,
            pre_step_tx: None,
            trace_tx: sctx.trace_tx.clone(),
            agent_name: Some("review".to_string()),
            policy: std::sync::Arc::new(react_core::agent::DefaultPolicy),
            llm: sctx.llm.clone(),
            storage: sctx.storage.clone(),
            scope: sctx.scope.clone(),
            keyspace: sctx.keyspace.clone(),
            query: sctx.query.clone(),
            dbt: sctx.dbt.clone(),
            vector: sctx.vector.clone(),
            thread_store: Some(thread_store),
            runtime: sctx
                .resolved_config
                .clone()
                .map(|c| c as Arc<dyn std::any::Any + Send + Sync>),
        };

        let prompt = Self::inject_review_question(question);
        match Agent::run_until_block(&registry, &actx, &sys, &tools_card, &prompt).await {
            Ok(RunOutcome::Final { thread_id: _tid, result }) => Ok(vec![FlowFrame::Final {
                answer: result.answer,
                sql: result.sql,
            }]),
            Ok(RunOutcome::AwaitUser { thread_id: _tid, prompt }) => Ok(vec![FlowFrame::AwaitUser { prompt }]),
            Ok(RunOutcome::AwaitApproval { thread_id: _tid, prompt }) => Ok(vec![FlowFrame::AwaitApproval { prompt }]),
            Err(e) => Err(e),
        }
    }

    fn agent_tool_ctx(thread_id: &str, sctx: &SuiteCtx) -> AgentCtx {
        let thread_store = ThreadStore::new(sctx.storage.clone(), sctx.scope.clone(), sctx.keyspace.clone());
        AgentCtx {
            top_k: 30,
            per_step_timeout_secs: 10,
            max_steps: 1,
            thread_id: Some(thread_id.to_string()),
            progress_tx: None,
            pre_step_tx: None,
            trace_tx: sctx.trace_tx.clone(),
            agent_name: Some("agent".to_string()),
            policy: std::sync::Arc::new(react_core::agent::DefaultPolicy),
            llm: sctx.llm.clone(),
            storage: sctx.storage.clone(),
            scope: sctx.scope.clone(),
            keyspace: sctx.keyspace.clone(),
            query: sctx.query.clone(),
            dbt: sctx.dbt.clone(),
            vector: sctx.vector.clone(),
            thread_store: Some(thread_store),
            runtime: sctx
                .resolved_config
                .clone()
                .map(|c| c as Arc<dyn std::any::Any + Send + Sync>),
        }
    }

    async fn run_agent(thread_id: &str, question: &str, sctx: &SuiteCtx) -> Result<Vec<FlowFrame>, String> {
        use control_flow::{DerivedGuardState, Phase};
        Self::ensure_catalog_bootstrap(sctx).await;

        let max_phase_steps: usize = std::env::var("AGENT_MAX_PHASE_STEPS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(24)
            .max(6)
            .min(200);

        let thread_store = ThreadStore::new(sctx.storage.clone(), sctx.scope.clone(), sctx.keyspace.clone());

        let mut out_frames: Vec<FlowFrame> = Vec::new();

        for _ in 0..max_phase_steps {
            let log = thread_store.get(thread_id).await;
            let phase = control_flow::phase_from_log(log.as_ref());
            let guard: DerivedGuardState = control_flow::derive_guard_state(log.as_ref());
            let allow_ask_approval = Self::allow_ask_approval_in_phase(log.as_ref(), phase);

            // Helper: most recent dbt_validate error brief (for prompt grounding).
            let mut last_validate_brief: Option<String> = None;
            if let Some(ref l) = log {
                for step in l.steps.iter().rev() {
                    if step.action != "dbt_validate" {
                        continue;
                    }
                    let errs: Vec<String> = step
                        .observation
                        .get("errors")
                        .and_then(|v| serde_json::from_value(v.clone()).ok())
                        .unwrap_or_default();
                    if !errs.is_empty() {
                        last_validate_brief = Some(dbt_error::compact_brief(&errs, 6, 1200));
                    }
                    break;
                }
            }

            match phase {
                Phase::Preflight => {
                    // Require core providers.
                    if sctx.query.is_none() {
                        return Ok(vec![FlowFrame::AwaitUser {
                            prompt: "Data Engineer agent requires a QueryProvider (e.g. Athena) configured. Configure providers.athena and restart.".to_string(),
                        }]);
                    }
                    if sctx.dbt.is_none() {
                        return Ok(vec![FlowFrame::AwaitUser {
                            prompt: "Data Engineer agent requires a DBT provider configured. Enable providers.dbt and restart.".to_string(),
                        }]);
                    }
                    // Ensure minimal dbt project exists.
                    if let Some(dbt) = sctx.dbt.as_ref() {
                        if let Err(e) = dbt.ensure_minimal_project(&sctx.scope).await {
                            let key = sctx.keyspace.dbt_project_key(&sctx.scope);
                            return Ok(vec![FlowFrame::AwaitUser {
                                prompt: format!(
                                    "Failed to create the DBT project in storage.\n\nExpected file:\n- {key}\n\nError:\n{e}\n\nThis is usually an S3 permission/prefix issue. Fix the runtime’s storage configuration/IAM permissions so it can write the project root, then retry."
                                ),
                            }]);
                        }
                    }
                    // Hard gate: dbt_project.yml MUST exist before we proceed, otherwise we will loop in authoring.
                    let key = sctx.keyspace.dbt_project_key(&sctx.scope);
                    match sctx.storage.head_etag(&key).await {
                        Ok(Some(_)) => {}
                        Ok(None) => {
                            return Ok(vec![FlowFrame::AwaitUser {
                                prompt: format!(
                                    "DBT project is incomplete: `dbt_project.yml` is missing in storage.\n\nExpected file:\n- {key}\n\nI can see models being written under `models/`, but without `dbt_project.yml` the suite cannot validate/build and will keep re-authoring.\n\nFix the runtime’s storage configuration/IAM permissions so it can write the DBT project root (not just `models/`), then retry."
                                ),
                            }]);
                        }
                        Err(e) => {
                            return Ok(vec![FlowFrame::AwaitUser {
                                prompt: format!(
                                    "Unable to verify presence of `dbt_project.yml` in storage.\n\nExpected file:\n- {key}\n\nError:\n{e}\n\nFix the runtime’s storage configuration/IAM permissions, then retry."
                                ),
                            }]);
                        }
                    }
                    // Persist transition.
                    control_flow::append_phase_with_reason(
                        &thread_store,
                        thread_id,
                        Some("agent".to_string()),
                        Some(Phase::Preflight),
                        Phase::CleanseAuthor,
                        Some("preflight_ok"),
                        Some(serde_json::json!({
                            "dbt_project_key": key,
                            "has_query_provider": sctx.query.is_some(),
                            "has_dbt_provider": sctx.dbt.is_some(),
                        })),
                    )
                    .await;
                    continue;
                }

                Phase::CleanseAuthor | Phase::ModelAuthor => {
                    let is_cleanse = phase == Phase::CleanseAuthor;
                    let sys = crate::util::time_context::with_time_context(if is_cleanse {
                        prompts::cleanse_system_prompt()
                    } else {
                        prompts::model_system_prompt()
                    });
                    let (registry, tools_card) = Self::build_tools_for_phase(phase, &guard, allow_ask_approval, sctx)?;
                    let actx = AgentCtx {
                        top_k: 30,
                        per_step_timeout_secs: 10,
                        max_steps: 60,
                        thread_id: Some(thread_id.to_string()),
                        progress_tx: None,
                        pre_step_tx: None,
                        trace_tx: sctx.trace_tx.clone(),
                        // IMPORTANT: always record a single agent label for agent-mode runs.
                        // Phase selection (cleanse vs model) is handled by the deterministic outer loop and prompts.
                        agent_name: Some("agent".to_string()),
                        // IMPORTANT: in agent-mode, the deterministic outer loop enforces validation/invariants.
                        // We still keep strict ask_user/ask_approval interrupts.
                        policy: std::sync::Arc::new(InterruptOnlyPolicy),
                        llm: sctx.llm.clone(),
                        storage: sctx.storage.clone(),
                        scope: sctx.scope.clone(),
                        keyspace: sctx.keyspace.clone(),
                        query: sctx.query.clone(),
                        dbt: sctx.dbt.clone(),
                        vector: sctx.vector.clone(),
                        thread_store: Some(thread_store.clone()),
                        runtime: sctx
                            .resolved_config
                            .clone()
                            .map(|c| c as Arc<dyn std::any::Any + Send + Sync>),
                    };

                    // Ground the next authoring pass with last validation summary (if any) and guard state.
                    let mut q = if is_cleanse {
                        Self::inject_cleanse_question(question)
                    } else {
                        Self::inject_model_question(question)
                    };
                    q.push_str("\n\nNOTE: In agent mode, validation and publish are handled by the suite phases. Do not call dbt_validate or publish tools; focus on authoring fixes and models.");
                    q.push_str("\nIMPORTANT: Tool-call argument shapes are strict. In particular: vect_query uses args.query_text (NOT args.query) and scope must be \"dataset\"|\"field\"|\"doc\"|\"artifact\"|\"metric\"|\"model\".");
                    q.push_str("\nIMPORTANT: sql_stats and sql_sample both require args.field. To sample rows, use run_sql with a LIMIT.");
                    if !allow_ask_approval {
                        q.push_str("\nIMPORTANT: Approval for this phase has already been recorded. You MUST NOT call ask_approval again; proceed with scaffolding.");
                    }
                    if let Some(b) = last_validate_brief.as_ref() {
                        q.push_str("\n\nLast dbt_validate summary (most recent):\n");
                        q.push_str(b);
                    }
                    // Surface the most recent suite-level guard block reason (if any) to help auto-fix.
                    if let Some(ref l) = log {
                        if let Some(last_block) = l.steps.iter().rev().find(|s| s.action == "guard_block") {
                            if let Some(reason) = last_block.observation.get("reason").and_then(|v| v.as_str()) {
                                if !reason.trim().is_empty() {
                                    q.push_str("\n\nSuite guard note (must resolve before validate):\n");
                                    q.push_str(reason.trim());
                                }
                            }
                        }
                    }
                    if guard.last_validate_failed && !guard.mutated_since_fail {
                        q.push_str("\n\nConstraint: your next steps must APPLY A MUTATING FIX before attempting dbt_validate again.");
                    }
                    if guard.probe_required && !guard.probe_satisfied {
                        q.push_str("\n\nConstraint: runtime validation failed after compile; you MUST run meaningful run_sql probes (not SELECT 1) to diagnose data before re-validating.");
                    }

                    // Deterministic invariant: do not allow leaving authoring without any models.
                    let has_models = control_flow::invariant_has_any_models(&actx).await.unwrap_or(false);
                    if !has_models {
                        q.push_str("\n\nIMPORTANT: invariant failed: there are no DBT model SQL files yet. Your first task is to create at least one staging model under models/ using staging_model or dbt_files op=patch.");
                    }

                    match Agent::run_until_block(&registry, &actx, &sys, &tools_card, &q).await {
                        Ok(RunOutcome::Final { .. }) => {
                            // Deterministic invariants: don't advance phases unless the project actually exists.
                            let has_proj = control_flow::invariant_has_dbt_project(&actx).await.unwrap_or(false);
                            let has_models = control_flow::invariant_has_any_models(&actx).await.unwrap_or(false);
                            if !has_proj || !has_models {
                                // Stay in the same authoring phase; the next pass will be prompted with invariant context.
                                continue;
                            }
                            // New guard: do not advance if this authoring phase has unresolved mutation/tool failures.
                            // Auto-loop in authoring so the agent can fix deterministically.
                            let latest_log = thread_store.get(thread_id).await;
                            match control_flow::gate_authoring_completion(latest_log.as_ref(), phase) {
                                control_flow::AuthoringGate::Allow => {}
                                control_flow::AuthoringGate::AwaitUser { prompt } => {
                                    return Ok(vec![FlowFrame::AwaitUser { prompt }]);
                                }
                                control_flow::AuthoringGate::Block { reason } => {
                                    let ts = chrono::Utc::now().to_rfc3339();
                                    let step = react_core::session::ThreadStep {
                                        action: "guard_block".to_string(),
                                        args: serde_json::json!({"phase": phase.as_str(), "kind": "authoring_completion"}),
                                        observation: serde_json::json!({"ok": false, "reason": reason.clone()}),
                                        ts,
                                        agent: Some("agent".to_string()),
                                    };
                                    let _ = thread_store.append_step(thread_id, step.clone()).await;
                                    let trigger_step_idx = thread_store
                                        .get(thread_id)
                                        .await
                                        .map(|l| l.steps.len().saturating_sub(1))
                                        .unwrap_or(0);
                                    control_flow::append_phase_with_reason(
                                        &thread_store,
                                        thread_id,
                                        Some("agent".to_string()),
                                        Some(phase),
                                        phase,
                                        Some("phase_blocked"),
                                        Some(serde_json::json!({
                                            "kind": "authoring_completion",
                                            "reason": reason,
                                            "trigger_step_idx": trigger_step_idx,
                                            "trigger_step": step,
                                        })),
                                    )
                                    .await;
                                    continue;
                                }
                            }
                            // Hard gate: if validate previously failed, do not advance unless a successful mutation
                            // (and any required probes) have been recorded since that failure.
                            let latest_log = thread_store.get(thread_id).await;
                            match control_flow::gate_authoring_to_validate(latest_log.as_ref()) {
                                control_flow::AuthoringGate::Allow => {}
                                control_flow::AuthoringGate::AwaitUser { prompt } => {
                                    return Ok(vec![FlowFrame::AwaitUser { prompt }]);
                                }
                                control_flow::AuthoringGate::Block { reason } => {
                                    let ts = chrono::Utc::now().to_rfc3339();
                                    let step = react_core::session::ThreadStep {
                                        action: "guard_block".to_string(),
                                        args: serde_json::json!({"phase": phase.as_str(), "kind": "authoring_to_validate"}),
                                        observation: serde_json::json!({"ok": false, "reason": reason.clone()}),
                                        ts,
                                        agent: Some("agent".to_string()),
                                    };
                                    let _ = thread_store.append_step(thread_id, step.clone()).await;
                                    let trigger_step_idx = thread_store
                                        .get(thread_id)
                                        .await
                                        .map(|l| l.steps.len().saturating_sub(1))
                                        .unwrap_or(0);
                                    control_flow::append_phase_with_reason(
                                        &thread_store,
                                        thread_id,
                                        Some("agent".to_string()),
                                        Some(phase),
                                        phase,
                                        Some("phase_blocked"),
                                        Some(serde_json::json!({
                                            "kind": "authoring_to_validate",
                                            "reason": reason,
                                            "trigger_step_idx": trigger_step_idx,
                                            "trigger_step": step,
                                        })),
                                    )
                                    .await;
                                    continue;
                                }
                            }
                            let to_phase = if is_cleanse { Phase::CleanseValidate } else { Phase::ModelValidate };
                            control_flow::append_phase_with_reason(
                                &thread_store,
                                thread_id,
                                Some("agent".to_string()),
                                Some(phase),
                                to_phase,
                                Some("authoring_complete"),
                                Some(serde_json::json!({
                                    "invariants": {
                                        "has_dbt_project_yml": has_proj,
                                        "has_any_models": has_models,
                                    },
                                    "guard_state": {
                                        "last_validate_failed": guard.last_validate_failed,
                                        "mutated_since_fail": guard.mutated_since_fail,
                                        "mutation_failures_since_validate": guard.mutation_failures_since_validate,
                                        "probe_required": guard.probe_required,
                                        "probe_satisfied": guard.probe_satisfied,
                                    }
                                })),
                            )
                            .await;
                            continue;
                        }
                        Ok(RunOutcome::AwaitUser { prompt, .. }) => return Ok(vec![FlowFrame::AwaitUser { prompt }]),
                        Ok(RunOutcome::AwaitApproval { prompt, .. }) => return Ok(vec![FlowFrame::AwaitApproval { prompt }]),
                        Err(e) => return Err(e),
                    }
                }

                Phase::CleanseValidate | Phase::ModelValidate => {
                    let actx = Self::agent_tool_ctx(thread_id, sctx);
                    // Deterministic validate (NO repair loop / no mutation).
                    let obs = control_flow::DeterministicDbtValidateOnce::run(&actx, true, false, None).await?;
                    let _ = thread_store
                        .append_step(
                            thread_id,
                            react_core::session::ThreadStep {
                                action: "dbt_validate".to_string(),
                                args: serde_json::json!({"build": true}),
                                observation: obs.clone(),
                                ts: chrono::Utc::now().to_rfc3339(),
                                agent: Some("agent".to_string()),
                            },
                        )
                        .await;

                    let ok = obs.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
                    let compile_ok = obs.get("compile_ok").and_then(|v| v.as_bool()).unwrap_or(false);
                    let run_ok = obs.get("run_ok").and_then(|v| v.as_bool()).unwrap_or(false);
                    if ok && compile_ok && run_ok {
                        let trigger_step_idx = thread_store
                            .get(thread_id)
                            .await
                            .map(|l| l.steps.len().saturating_sub(1))
                            .unwrap_or(0);
                        let to_phase = if phase == Phase::CleanseValidate { Phase::CleanseReview } else { Phase::ModelReview };
                        control_flow::append_phase_with_reason(
                            &thread_store,
                            thread_id,
                            Some("agent".to_string()),
                            Some(phase),
                            to_phase,
                            Some("validate_pass"),
                            Some(serde_json::json!({
                                "dbt_validate_observation": obs,
                                "dbt_validate_step_idx": trigger_step_idx,
                            })),
                        )
                        .await;
                        continue;
                    }

                    // Warehouse config failures require user action.
                    let errs: Vec<String> = obs
                        .get("errors")
                        .and_then(|v| serde_json::from_value(v.clone()).ok())
                        .unwrap_or_default();
                    let class = dbt_error::classify(&errs);
                    if matches!(class, dbt_error::DbtErrorClass::WarehouseConfig) {
                        let brief = dbt_error::compact_brief(&errs, 6, 1400);
                        return Ok(vec![FlowFrame::AwaitUser {
                            prompt: format!(
                                "dbt_validate failed due to a warehouse/AWS configuration issue. Fix the configuration and then click Approve/Continue.\n\nError summary:\n{}",
                                brief
                            ),
                        }]);
                    }

                    // Validation failed -> go back to corresponding author phase.
                    let trigger_step_idx = thread_store
                        .get(thread_id)
                        .await
                        .map(|l| l.steps.len().saturating_sub(1))
                        .unwrap_or(0);
                    let to_phase = if phase == Phase::CleanseValidate { Phase::CleanseAuthor } else { Phase::ModelAuthor };
                    control_flow::append_phase_with_reason(
                        &thread_store,
                        thread_id,
                        Some("agent".to_string()),
                        Some(phase),
                        to_phase,
                        Some("validate_fail"),
                        Some(serde_json::json!({
                            "dbt_validate_observation": obs,
                            "dbt_validate_step_idx": trigger_step_idx,
                            "errors": errs,
                        })),
                    )
                    .await;
                    continue;
                }

                Phase::CleanseReview | Phase::ModelReview | Phase::PostPublishReview => {
                    let review_q = Self::build_review_question_with_context(question, phase, log.as_ref());
                    let frames = Self::run_review(thread_id, &review_q, sctx).await?;
                    let first = frames.into_iter().next().unwrap_or(FlowFrame::Final { answer: "".to_string(), sql: None });
                    let (answer, _sql) = match first {
                        FlowFrame::Final { answer, sql } => (answer, sql),
                        other => return Ok(vec![other]),
                    };

                    // Best-effort: capture the review final step as the trigger for phase transitions.
                    let (trigger_step_idx, trigger_step) = thread_store
                        .get(thread_id)
                        .await
                        .and_then(|l| {
                            let idx = l.steps.len().saturating_sub(1);
                            l.steps.last().cloned().map(|s| (idx, s))
                        })
                        .unwrap_or((0, react_core::session::ThreadStep {
                            action: "unknown".to_string(),
                            args: serde_json::Value::Null,
                            observation: serde_json::json!({"ok": false, "reason":"missing thread step"}),
                            ts: chrono::Utc::now().to_rfc3339(),
                            agent: Some("agent".to_string()),
                        }));

                    let meta = Self::parse_review_meta(&answer).unwrap_or(ReviewMeta {
                        actionable: false,
                        dataset_ids: vec![],
                        tier: "unknown".to_string(),
                    });
                    out_frames.push(FlowFrame::Review {
                        text: answer.clone(),
                        meta: Some(serde_json::json!({
                            "actionable": meta.actionable,
                            "dataset_ids": meta.dataset_ids.clone(),
                            "tier": meta.tier.clone(),
                        })),
                    });

                    if !meta.actionable {
                        // Move forward in the deterministic pipeline.
                        let next = match phase {
                            Phase::CleanseReview => Phase::ModelAuthor,
                            Phase::ModelReview => Phase::PublishAwaitApproval,
                            Phase::PostPublishReview => Phase::Done,
                            _ => Phase::Done,
                        };
                        control_flow::append_phase_with_reason(
                            &thread_store,
                            thread_id,
                            Some("agent".to_string()),
                            Some(phase),
                            next,
                            Some("review_actionable_false"),
                            Some(serde_json::json!({
                                "review_phase": phase.as_str(),
                                "meta": {
                                    "actionable": meta.actionable,
                                    "dataset_ids": meta.dataset_ids,
                                    "tier": meta.tier,
                                },
                                "answer": answer,
                                "trigger_step_idx": trigger_step_idx,
                                "trigger_step": trigger_step,
                            })),
                        )
                        .await;
                        continue;
                    }

                    // Actionable review -> route back to appropriate authoring phase.
                    let tier = meta.tier.trim().to_lowercase();
                    let back = if tier == "silver" {
                        Phase::CleanseAuthor
                    } else {
                        Phase::ModelAuthor
                    };
                    control_flow::append_phase_with_reason(
                        &thread_store,
                        thread_id,
                        Some("agent".to_string()),
                        Some(phase),
                        back,
                        Some("review_actionable_true"),
                        Some(serde_json::json!({
                            "review_phase": phase.as_str(),
                            "meta": {
                                "actionable": meta.actionable,
                                "dataset_ids": meta.dataset_ids,
                                "tier": meta.tier,
                            },
                            "answer": answer,
                            "trigger_step_idx": trigger_step_idx,
                            "trigger_step": trigger_step,
                        })),
                    )
                    .await;
                    continue;
                }

                Phase::PublishAwaitApproval => {
                    // If the most recent persisted user action is "reject", stop and ask for guidance.
                    if let Some(ref l) = log {
                        if let Some(last_user) = l.steps.iter().rev().find(|s| s.action == "user") {
                            let t = last_user.args.get("text").and_then(|v| v.as_str()).unwrap_or("").trim().to_lowercase();
                            if t == "reject" {
                                return Ok(vec![FlowFrame::AwaitUser {
                                    prompt: "Publish was rejected. Provide guidance (e.g. restrict dataset_ids, change materializations, or adjust models) and then re-run agent.".to_string(),
                                }]);
                            }
                            if t == "approve" {
                                control_flow::append_phase_with_reason(
                                    &thread_store,
                                    thread_id,
                                    Some("agent".to_string()),
                                    Some(Phase::PublishAwaitApproval),
                                    Phase::Publish,
                                    Some("user_approved_publish"),
                                    Some(serde_json::json!({
                                        "last_user_step": last_user,
                                    })),
                                )
                                .await;
                                continue;
                            }
                        }
                    }

                    let actx = Self::agent_tool_ctx(thread_id, sctx);
                    let tool = tools::publish_dbt_to_provider::PublishDbtToProviderTool { datasets: sctx.datasets.clone(), catalog: sctx.catalog.clone() };
                    let obs = control_flow::call_and_record_tool(
                        &thread_store,
                        thread_id,
                        Some("agent".to_string()),
                        &tool,
                        serde_json::json!({}),
                        &actx,
                        60,
                    )
                    .await;
                    let ok = obs.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
                    let stage = obs.get("stage").and_then(|v| v.as_str()).unwrap_or("");
                    if ok && (stage == "published" || stage == "no_change") {
                        control_flow::append_phase_with_reason(
                            &thread_store,
                            thread_id,
                            Some("agent".to_string()),
                            Some(Phase::PublishAwaitApproval),
                            Phase::PostPublishReview,
                            Some("publish_success"),
                            Some(serde_json::json!({
                                "publish_observation": obs,
                            })),
                        )
                        .await;
                        continue;
                    }
                    if ok && stage == "await_approval" {
                        let prompt = obs.get("prompt").and_then(|v| v.as_str()).unwrap_or("Approve to publish?").to_string();
                        return Ok(vec![FlowFrame::AwaitApproval { prompt }]);
                    }
                    // Publish failed: send back to model authoring to fix.
                    control_flow::append_phase_with_reason(
                        &thread_store,
                        thread_id,
                        Some("agent".to_string()),
                        Some(Phase::PublishAwaitApproval),
                        Phase::ModelAuthor,
                        Some("publish_fail"),
                        Some(serde_json::json!({
                            "publish_observation": obs,
                        })),
                    )
                    .await;
                    continue;
                }

                Phase::Publish => {
                    // Only proceed if the last user action is "approve".
                    if let Some(ref l) = log {
                        if let Some(last_user) = l.steps.iter().rev().find(|s| s.action == "user") {
                            let t = last_user.args.get("text").and_then(|v| v.as_str()).unwrap_or("").trim().to_lowercase();
                            if t != "approve" {
                                return Ok(vec![FlowFrame::AwaitApproval {
                                    prompt: "Publish requires explicit approval. Click Approve to continue.".to_string(),
                                }]);
                            }
                        }
                    }
                    let actx = Self::agent_tool_ctx(thread_id, sctx);
                    let tool = tools::publish_dbt_to_provider::PublishDbtToProviderTool { datasets: sctx.datasets.clone(), catalog: sctx.catalog.clone() };
                    let obs = control_flow::call_and_record_tool(
                        &thread_store,
                        thread_id,
                        Some("agent".to_string()),
                        &tool,
                        serde_json::json!({"confirm": true}),
                        &actx,
                        600,
                    )
                    .await;
                    let ok = obs.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
                    let stage = obs.get("stage").and_then(|v| v.as_str()).unwrap_or("");
                    if ok && (stage == "published" || stage == "no_change") {
                        control_flow::append_phase_with_reason(
                            &thread_store,
                            thread_id,
                            Some("agent".to_string()),
                            Some(Phase::Publish),
                            Phase::PostPublishReview,
                            Some("publish_confirmed_success"),
                            Some(serde_json::json!({
                                "publish_observation": obs,
                            })),
                        )
                        .await;
                        continue;
                    }
                    // Failed publish -> back to model authoring.
                    control_flow::append_phase_with_reason(
                        &thread_store,
                        thread_id,
                        Some("agent".to_string()),
                        Some(Phase::Publish),
                        Phase::ModelAuthor,
                        Some("publish_confirmed_fail"),
                        Some(serde_json::json!({
                            "publish_observation": obs,
                        })),
                    )
                    .await;
                    continue;
                }

                Phase::Done => {
                    let mut answer = "Agent flow completed (deterministic phases): cleanse → validate → review → model → validate → review → publish → review.\n".to_string();
                    if let Some(last) = out_frames.iter().rev().find_map(|f| match f {
                        FlowFrame::Review { text, .. } => Some(text.clone()),
                        _ => None,
                    }) {
                        answer.push_str("\nLatest review summary:\n");
                        answer.push_str(&last);
                    }
                    out_frames.push(FlowFrame::Final { answer, sql: None });
                    return Ok(out_frames);
                }
            }
        }

        Ok(vec![FlowFrame::AwaitUser {
            prompt: "Agent reached max phase transitions without completing. Please review thread history and retry with more specific instructions.".to_string(),
        }])
    }

    async fn run_authoring(kind: AuthoringKind, thread_id: &str, question: &str, sctx: &SuiteCtx) -> Result<Vec<FlowFrame>, String> {
        Self::ensure_catalog_bootstrap(sctx).await;

        let (agent_name, sys, tools_card, run_preflight_on_bundle) = match kind {
            AuthoringKind::Cleanse => (
                "cleanse",
                crate::util::time_context::with_time_context(prompts::cleanse_system_prompt()),
                prompts::cleanse_tool_card(),
                false,
            ),
            AuthoringKind::Model => (
                "model",
                crate::util::time_context::with_time_context(prompts::model_system_prompt()),
                prompts::model_tool_card(),
                true,
            ),
        };

        let pf = crate::preflight::CatalogPreflightProvider {
            discovery_limits: crate::preflight::discovery::DiscoveryLimits::default(),
            run_preflight_on_bundle,
        };
        let bundle = pf.run(thread_id, question, agent_name, sctx).await.discovery;

        let registry = Self::build_tools(agent_name, sctx)?;
        let thread_store = ThreadStore::new(sctx.storage.clone(), sctx.scope.clone(), sctx.keyspace.clone());

        let actx = AgentCtx {
            top_k: 30,
            per_step_timeout_secs: 10,
            max_steps: 50,
            thread_id: Some(thread_id.to_string()),
            progress_tx: None,
            pre_step_tx: None,
            trace_tx: sctx.trace_tx.clone(),
            agent_name: Some(agent_name.to_string()),
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
            runtime: sctx
                .resolved_config
                .clone()
                .map(|c| c as Arc<dyn std::any::Any + Send + Sync>),
        };

        let mut last_final: Option<react_core::session::ThreadResult> = None;
        let mut prompt = match kind {
            AuthoringKind::Cleanse => Self::inject_cleanse_question(question),
            AuthoringKind::Model => Self::inject_model_question(question),
        };

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

                    let runtime_failures: Vec<serde_json::Value> = obs
                        .get("runtime_failures")
                        .and_then(|v| v.as_array())
                        .cloned()
                        .unwrap_or_default();
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

                    if compile_ok && !run_ok && !runtime_failures.is_empty() {
                        let mut lines: Vec<String> = Vec::new();
                        for rf in runtime_failures.iter().take(3) {
                            let name = rf.get("name").and_then(|v| v.as_str()).unwrap_or("unknown_test");
                            let n = rf.get("failures").and_then(|v| v.as_u64()).map(|x| x.to_string()).unwrap_or("?".to_string());
                            let mh = rf.get("model_hint").and_then(|v| v.as_str()).unwrap_or("");
                            let ch = rf.get("column_hint").and_then(|v| v.as_str()).unwrap_or("");
                            let hint = if !mh.is_empty() && !ch.is_empty() {
                                format!(" (model_hint={}, column_hint={})", mh, ch)
                            } else if !mh.is_empty() {
                                format!(" (model_hint={})", mh)
                            } else {
                                "".to_string()
                            };
                            lines.push(format!("- {} failures: {}{}", name, n, hint));
                        }
                        let manifest_lines = Self::manifest_targeting_lines(&actx, &runtime_failures).await;
                        let manifest_block = if manifest_lines.is_empty() {
                            "".to_string()
                        } else {
                            format!("\nManifest targeting:\n{}\n", manifest_lines.join("\n"))
                        };
                        prompt = format!(
                            "Auto-remediation attempt {}: dbt build failed at runtime (tests) AFTER a successful compile.\n\
                             Failing tests:\n{}\n{}\
                             IMPORTANT: Your very next step MUST be a FIX to dbt artifacts (prefer fixing staging/silver models; do NOT relax/remove tests unless nullable-by-design is justified).\n\
                             Recommended flow:\n\
                             - Use `dbt_files op=manifest_find` (or `dbt_files op=get_json`) to target `target/manifest.json` WITHOUT dumping the full file.\n\
                               - Find the failing test node(s) by name, then find the referenced model node via depends_on.\n\
                               - From the model node, compute the physical relation: <database>.<schema>.<alias>.\n\
                             - Use `sql_schema` on that relation to determine the tested column type.\n\
                             - Use `run_sql` to probe the actual data before editing:\n\
                               - Null check: SELECT count(*) AS total, count_if({{col}} IS NULL) AS nulls FROM {{relation}}\n\
                               - If string-ish: SELECT count_if(trim(cast({{col}} AS varchar)) = '') AS empty FROM {{relation}}\n\
                               - If time-like by type: SELECT count_if(try_cast(nullif(trim(cast({{col}} AS varchar)), '') AS timestamp) IS NULL) AS unparseable FROM {{relation}}\n\
                               - Sample failing: SELECT {{col}} FROM {{relation}} WHERE {{col}} IS NULL LIMIT 50\n\
                             - Apply a fix using `staging_model` or `dbt_files op=patch`.\n\
                             - You MUST NOT claim fixed unless a probe query shows the failure condition is now 0 rows.\n\
                             Only AFTER applying a fix should you re-run `dbt_validate` with build=true.",
                            attempt + 1,
                            lines.join("\n"),
                            manifest_block
                        );
                    } else {
                        prompt = format!(
                            "Auto-remediation attempt {}: dbt_validate/build failed.\n\nError summary:\n{}\n\nAutomatically fix the DBT project:\n- Prefer calling `staging_model` to update staging/silver models (nested fields, cleansing, naming).\n- Use dbt_files or artifacts to inspect/edit existing files (preview_diff if helpful).\n- Re-run dbt_validate with build=true.\nRepeat until compile_ok=true AND run_ok=true.",
                            attempt + 1,
                            brief
                        );
                    }
                    continue;
                }
                Ok(RunOutcome::AwaitUser { thread_id: _tid, prompt: p }) => {
                    return Ok(vec![FlowFrame::AwaitUser { prompt: p }]);
                }
                Ok(RunOutcome::AwaitApproval { thread_id: _tid, prompt: p }) => {
                    return Ok(vec![FlowFrame::AwaitApproval { prompt: p }]);
                }
                Err(e) => return Err(e),
            }
        }

        if let Some(r) = last_final {
            return Ok(vec![FlowFrame::Final { answer: r.answer, sql: r.sql }]);
        }
        Err(format!("{}: no outcome", agent_name))
    }

    async fn run_cleanse(thread_id: &str, question: &str, sctx: &SuiteCtx) -> Result<Vec<FlowFrame>, String> {
        Self::run_authoring(AuthoringKind::Cleanse, thread_id, question, sctx).await
    }

    async fn run_model(thread_id: &str, question: &str, sctx: &SuiteCtx) -> Result<Vec<FlowFrame>, String> {
        Self::run_authoring(AuthoringKind::Model, thread_id, question, sctx).await
    }
}

#[async_trait]
impl Suite for DataEngineerSuite {
    fn id(&self) -> &'static str {
        "data_engineer"
    }

    fn phase_order(&self, agent_type: &str) -> Vec<String> {
        // Only expose phases for agent-mode; other modes are single-pass.
        if agent_type != "agent" {
            return Vec::new();
        }
        use crate::data_engineer::control_flow::Phase;
        vec![
            Phase::Preflight.as_str(),
            Phase::CleanseAuthor.as_str(),
            Phase::CleanseValidate.as_str(),
            Phase::CleanseReview.as_str(),
            Phase::ModelAuthor.as_str(),
            Phase::ModelValidate.as_str(),
            Phase::ModelReview.as_str(),
            Phase::PublishAwaitApproval.as_str(),
            Phase::Publish.as_str(),
            Phase::PostPublishReview.as_str(),
            Phase::Done.as_str(),
        ]
        .into_iter()
        .map(|s| s.to_string())
        .collect()
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
            "agent" => Self::run_agent(thread_id, question, ctx).await,
            "model" => Self::run_model(thread_id, question, ctx).await,
            "cleanse" => Self::run_cleanse(thread_id, question, ctx).await,
            "review" => Self::run_review(thread_id, question, ctx).await,
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
            "agent" => Self::run_agent(thread_id, question, ctx).await,
            "model" => Self::run_model(thread_id, question, ctx).await,
            "cleanse" => Self::run_cleanse(thread_id, question, ctx).await,
            "review" => Self::run_review(thread_id, question, ctx).await,
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
            "agent" => Self::run_agent(thread_id, text, ctx).await,
            "model" => Self::run_model(thread_id, text, ctx).await,
            "cleanse" => Self::run_cleanse(thread_id, text, ctx).await,
            "review" => Self::run_review(thread_id, text, ctx).await,
            _ => Self::run_ask(thread_id, text, ctx).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Arc;

    struct MockQuery;

    #[async_trait]
    impl react_core::providers::QueryProvider for MockQuery {
        async fn query(&self, _sql: &str) -> Result<react_core::providers::QueryResult, String> {
            Ok(react_core::providers::QueryResult { header: vec![], rows: vec![], meta: None })
        }
        async fn schema(&self, _dataset_fqn: &str) -> Result<Vec<(String, String)>, String> {
            Ok(vec![])
        }
        async fn sample(&self, _dataset_fqn: &str, _limit: usize) -> Result<Vec<Vec<String>>, String> {
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn review_registry_is_read_only() {
        let mut sctx = SuiteCtx::default();
        sctx.query = Some(Arc::new(MockQuery));

        let reg = DataEngineerSuite::build_tools("review", &sctx).expect("build_tools(review) should succeed");
        let actx = DataEngineerSuite::agent_tool_ctx("t", &sctx);

        // Not allowed in review
        assert!(reg.call("approve_and_save_artifact", serde_json::json!({}), &actx).await.is_err());
        assert!(reg.call("approve_and_save_artifact_batch", serde_json::json!({}), &actx).await.is_err());
        assert!(reg.call("dbt_validate", serde_json::json!({}), &actx).await.is_err());
        assert!(reg.call("publish_dbt_to_provider", serde_json::json!({}), &actx).await.is_err());
        assert!(reg.call("staging_model", serde_json::json!({}), &actx).await.is_err());
        assert!(reg.call("catalog_note", serde_json::json!({}), &actx).await.is_err());
        assert!(reg.call("ask_user", serde_json::json!({}), &actx).await.is_err());
        assert!(reg.call("ask_approval", serde_json::json!({}), &actx).await.is_err());

        // Also exclude arbitrary SQL execution in review mode.
        assert!(reg.call("run_sql", serde_json::json!({"sql":"SELECT 1"}), &actx).await.is_err());

        // Allowed in review
        let obs = reg.call("artifacts", serde_json::json!({"op":"list","limit":5}), &actx).await.expect("artifacts should be available");
        assert_eq!(obs.get("ok").and_then(|v| v.as_bool()), Some(true));
    }

    #[tokio::test]
    async fn agent_authoring_hard_mutation_phase_locks_tools() {
        let mut sctx = SuiteCtx::default();
        sctx.query = Some(Arc::new(MockQuery));
        let actx = DataEngineerSuite::agent_tool_ctx("t", &sctx);

        let guard = crate::data_engineer::control_flow::DerivedGuardState {
            last_validate_failed: true,
            mutated_since_fail: false,
            mutation_failures_since_validate: 0,
            probe_required: false,
            probe_satisfied: false,
        };
        let (reg, _card) = DataEngineerSuite::build_tools_for_phase(
            crate::data_engineer::control_flow::Phase::ModelAuthor,
            &guard,
            true,
            &sctx,
        )
            .expect("build_tools_for_phase should succeed");

        // run_sql should not be available in hard mutation-only mode
        assert!(reg.call("run_sql", serde_json::json!({"sql":"SELECT 1"}), &actx).await.is_err());

        // dbt_files get should be blocked (put-only wrapper)
        assert!(reg.call("dbt_files", serde_json::json!({"op":"get","path":"dbt_project.yml"}), &actx).await.is_err());
    }

    #[test]
    fn allow_ask_approval_is_monotonic_within_phase() {
        use crate::data_engineer::control_flow::Phase;
        use react_core::session::ThreadLog;

        let log = ThreadLog {
            steps: vec![
                react_core::session::ThreadStep {
                    action: "phase".to_string(),
                    args: serde_json::json!({"phase":"cleanse_author"}),
                    observation: serde_json::json!({"ok":true}),
                    ts: "t".to_string(),
                    agent: Some("agent".to_string()),
                },
                react_core::session::ThreadStep {
                    action: "ask_approval".to_string(),
                    args: serde_json::json!({"prompt":"p"}),
                    observation: serde_json::json!({"ok":true}),
                    ts: "t".to_string(),
                    agent: Some("agent".to_string()),
                },
                react_core::session::ThreadStep {
                    action: "user".to_string(),
                    args: serde_json::json!({"text":"approve"}),
                    observation: serde_json::json!({"ok":true}),
                    ts: "t".to_string(),
                    agent: Some("agent".to_string()),
                },
                // Later user chatter must not re-enable ask_approval.
                react_core::session::ThreadStep {
                    action: "user".to_string(),
                    args: serde_json::json!({"text":"continue"}),
                    observation: serde_json::json!({"ok":true}),
                    ts: "t".to_string(),
                    agent: Some("agent".to_string()),
                },
            ],
            result: None,
            title: None,
            title_finalized: false,
        };
        assert_eq!(DataEngineerSuite::allow_ask_approval_in_phase(Some(&log), Phase::CleanseAuthor), false);
    }

    #[test]
    fn review_question_includes_prior_review_and_mutation_diff_when_available() {
        use crate::data_engineer::control_flow::Phase;
        use react_core::session::{ThreadLog, ThreadStep};

        let prior_review_answer = "META:{\"actionable\":true,\"dataset_ids\":[\"x\"],\"tier\":\"silver\"}\n\nPlease add tests.";
        let prior_review_transition = ThreadStep {
            action: "phase".to_string(),
            args: serde_json::json!({
                "phase":"cleanse_author",
                "from_phase":"cleanse_review",
                "reason_code":"review_actionable_true",
                "reason_detail": {
                    "review_phase":"cleanse_review",
                    "meta": {"actionable": true, "dataset_ids": ["x"], "tier":"silver"},
                    "answer": prior_review_answer
                }
            }),
            observation: serde_json::json!({"ok":true}),
            ts: "t".to_string(),
            agent: Some("agent".to_string()),
        };

        let log = ThreadLog {
            steps: vec![
                prior_review_transition,
                ThreadStep {
                    action: "staging_model".to_string(),
                    args: serde_json::json!({"dataset_ids":["AwsDataCatalog.test_raw.raw_orders"]}),
                    observation: serde_json::json!({"ok": true, "written_keys":["k1"]}),
                    ts: "t".to_string(),
                    agent: Some("agent".to_string()),
                },
                ThreadStep {
                    action: "phase".to_string(),
                    args: serde_json::json!({"phase":"cleanse_review","from_phase":"cleanse_validate","reason_code":"validate_pass","reason_detail":{"dbt_validate_step_idx": 1}}),
                    observation: serde_json::json!({"ok":true}),
                    ts: "t".to_string(),
                    agent: Some("agent".to_string()),
                },
            ],
            result: None,
            title: None,
            title_finalized: false,
        };

        let q = DataEngineerSuite::build_review_question_with_context("orig goal", Phase::CleanseReview, Some(&log));
        assert!(q.contains("Review context"), "should include context header");
        assert!(q.contains("Previous review decision"), "should include prior review block");
        assert!(q.contains("What changed since previous review"), "should include mutation diff");
        assert!(q.contains("staging_model"), "should mention mutation action");
        assert!(q.contains("validate_pass"), "should include entry reason");
        assert!(q.contains("Original goal"), "should retain original goal section");
    }
}

