use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::data_engineer_shared::policy_sql_validated::SqlValidatedPolicy;
use crate::data_engineer_shared::types::DatasetCandidate;
use crate::flow_frame::FlowFrame;
use crate::preflight::PreflightProvider;
use crate::suite::{Suite, SuiteCtx};
use react_core::agent::{Agent, AgentCtx, AgentPolicy, Interrupt, RunOutcome};
use react_core::llm::LlmCallOptions;
use react_core::session::ThreadStore;
use react_core::tools::{Tool, ToolRegistry};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

pub struct DataEngineerSuite;

pub mod control_flow;
pub mod dataset_truth;
pub mod dbt_error;
pub mod dbt_repair;
pub mod facts;
pub mod naming;
pub mod patch_protocol;
pub mod plan;
pub mod project_files;
pub mod project_fs;
pub mod schema_policy;
pub mod prompts;
mod review_batched;
pub mod tools;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ValidateFailureClass {
    SqlOrRuntime,
    SchemaOrPrecheck,
    Unknown,
}

fn classify_validate_failure(
    entered_from_precheck_failed: bool,
    last_validate_brief: Option<&str>,
    last_guard_reason: Option<&str>,
) -> ValidateFailureClass {
    if entered_from_precheck_failed {
        return ValidateFailureClass::SchemaOrPrecheck;
    }
    let mut hay = String::new();
    if let Some(b) = last_validate_brief {
        hay.push_str(b);
        hay.push('\n');
    }
    if let Some(r) = last_guard_reason {
        hay.push_str(r);
    }
    let t = hay.to_ascii_lowercase();
    // Schema/precheck-style failures.
    if t.contains("precheck_failed")
        || t.contains("schema.yml")
        || t.contains(".yml")
        || t.contains(".yaml")
        || t.contains("yaml")
        || t.contains("schema contract")
        || t.contains("duplicate definition")
        || t.contains("duplicate definitions")
    {
        return ValidateFailureClass::SchemaOrPrecheck;
    }
    // SQL compile/runtime-style failures.
    if t.contains("compilation error")
        || t.contains("database error")
        || t.contains("runtime error")
        || t.contains("column_not_found")
        || t.contains("unresolved column")
        || t.contains("syntax error")
        || t.contains("parse error")
    {
        return ValidateFailureClass::SqlOrRuntime;
    }
    if t.trim().is_empty() {
        ValidateFailureClass::Unknown
    } else {
        // Default to SQL/runtime: when dbt fails, prefer fixing the failing SQL targets before new work.
        ValidateFailureClass::SqlOrRuntime
    }
}

fn lock_prompt_for_plan(
    kind: &str,
    plan_key: &str,
    consecutive: usize,
    total: usize,
    next_items: &[String],
    expected_paths: &[String],
) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "Plan-batched authoring is locked ({kind}).\n\n\
Reason:\n\
- consecutive_batch_failures = {consecutive} (limit {})\n\
- total_batch_failures = {total}\n\n\
Plan:\n\
- plan_key: {plan_key}\n",
        crate::data_engineer::tools::apply_next_batch::MAX_CONSECUTIVE_BATCH_FAILURES
    ));
    if !next_items.is_empty() {
        s.push_str("\nNext batch items:\n");
        for it in next_items.iter().take(6) {
            s.push_str("- ");
            s.push_str(it);
            s.push('\n');
        }
    }
    if !expected_paths.is_empty() {
        s.push_str("\nRecommended next step:\n");
        s.push_str(
            "- Apply a targeted `dbt_files op=patch` to fix the failing artifact(s):\n",
        );
        for p in expected_paths.iter().take(6) {
            s.push_str("  - ");
            s.push_str(p);
            s.push('\n');
        }
        s.push_str(
            "\nThen retry. This lock exists to prevent infinite loops when batch application repeatedly fails.\n",
        );
    } else {
        s.push_str(
            "\nRecommended next step:\n- Apply a targeted `dbt_files op=patch` to the failing DBT artifact(s), then retry.\n",
        );
    }
    s
}

/// Agent-mode policy: preserve strict interrupts (ask_user/ask_approval), but otherwise accept finals.
struct InterruptOnlyPolicy;

#[async_trait::async_trait]
impl AgentPolicy for InterruptOnlyPolicy {
    fn interrupt_for_action(
        &self,
        action_name: &str,
        args: &serde_json::Value,
        obs: &serde_json::Value,
    ) -> Option<Interrupt> {
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
        // Provide a timeout for *all* tools. Individual tools can override this baseline.
        //
        // Rationale:
        // - In headless/terminal mode we want runs to progress without spurious timeouts.
        // - Some tools wrap nested LLM calls + storage IO (schema/model batch tools).
        // - dbt validate/build/publish can take minutes in real environments.
        let baseline = 120;
        let secs = match action_name {
            // Can involve an inner LLM call + multiple writes.
            "staging_model" => 600,
            "apply_next_cleanse_batch" => 600,
            "apply_next_cleanse_schema_batch" => 600,

            // Can involve multiple storage reads + an inner LLM call + writes.
            "gold_model" => 300,
            "apply_next_model_batch" => 300,
            "apply_next_model_schema_batch" => 300,

            // dbt can be slow (compile/build/test) depending on environment.
            "dbt_validate" => 900,
            "publish_dbt_to_provider" => 900,

            // Warehouse queries can legitimately take >10s.
            "run_sql" => 120,

            // Writes can be larger.
            "approve_and_save_artifact_batch" => 120,
            "approve_and_save_artifact" => 120,

            // Storage reads/writes sometimes hit network latency.
            "dbt_files" => 120,

            // Discovery tools.
            "sql_schema" => 120,
            "sql_stats" => 120,
            "sql_sample" => 120,
            "vect_query" => 120,
            "vect_upsert" => 120,

            // Preflight can fan out and be slow depending on provider.
            "preflight_catalog_all" => 600,
            "preflight_catalog_dataset" => 300,
            "preflight_catalog_schema" => 300,

            _ => baseline,
        };
        Some(secs)
    }

    async fn handle_final(
        &self,
        tools: &ToolRegistry,
        ctx: &AgentCtx,
        transcript: &mut Vec<String>,
        store: Option<&react_core::session::ThreadStore>,
        thread_id: &str,
        final_env: &react_core::agent::FinalEnvelope,
    ) -> Result<Option<RunOutcome>, String> {
        react_core::agent::DefaultPolicy
            .handle_final(tools, ctx, transcript, store, thread_id, final_env)
            .await
    }
}

#[cfg(test)]
mod interrupt_only_policy_tests {
    use super::*;

    #[test]
    fn interrupt_only_policy_overrides_gold_model_timeout() {
        let p = InterruptOnlyPolicy;
        assert_eq!(p.timeout_for_tool("gold_model"), Some(300));
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

#[derive(Clone, Debug)]
enum AllowedBatch {
    CleanseDatasetIds(Vec<String>),
    ModelItemNames(Vec<String>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UserDecision {
    Approve,
    Reject,
}

impl DataEngineerSuite {
    fn plan_json_repair_system_prompt(kind: &str) -> String {
        match kind {
            "cleanse_plan" => crate::prompts::plan::cleanse_plan_system_prompt()
                + "\n\nSTRICT REPAIR MODE:\n\
- Tools are NOT available.\n\
- You MUST output ONLY a single final JSON envelope.\n\
- Do not output any prose.\n\
- Return: {\"final\":{\"kind\":\"cleanse_plan\",\"payload\":<json_plan_object>,\"display\":\"<optional short summary>\"}}\n",
            "model_plan" => crate::prompts::plan::model_plan_system_prompt()
                + "\n\nSTRICT REPAIR MODE:\n\
- Tools are NOT available.\n\
- You MUST output ONLY a single final JSON envelope.\n\
- Do not output any prose.\n\
- Return: {\"final\":{\"kind\":\"model_plan\",\"payload\":<json_plan_object>,\"display\":\"<optional short summary>\"}}\n",
            _ => {
                "You are repairing a JSON plan.\n\
Hard rules:\n\
- Tools are NOT available.\n\
- Output STRICT JSON only.\n"
                    .to_string()
            }
        }
    }

    /// Parse a user approval/rejection decision from free-form text.
    ///
    /// In agent-mode we accept a small set of loose synonyms so chat replies like
    /// "Approved", "ok", or "continue" don't trap the loop in repeated approval prompts.
    fn parse_user_decision(text: &str) -> Option<UserDecision> {
        let raw = text.trim();
        if raw.is_empty() {
            return None;
        }

        fn normalize_token(s: &str) -> Option<String> {
            let t = s
                .trim()
                // Strip surrounding punctuation (approve!, reject., etc).
                .trim_matches(|c: char| !c.is_ascii_alphanumeric())
                .to_lowercase();
            if t.is_empty() {
                None
            } else {
                Some(t)
            }
        }

        // Prefer first token so "yes please" still counts.
        let first = raw.split_whitespace().next().unwrap_or("");
        let tok = normalize_token(first).or_else(|| normalize_token(raw))?;

        match tok.as_str() {
            // Approve
            "approve" | "approved" | "yes" | "y" | "ok" | "okay" | "continue" => {
                Some(UserDecision::Approve)
            }
            // Reject
            "reject" | "rejected" | "no" | "n" => Some(UserDecision::Reject),
            _ => None,
        }
    }

    fn actionable_review_entry_step_idx(
        log: Option<&react_core::session::ThreadLog>,
        phase: control_flow::Phase,
    ) -> Option<usize> {
        let Some(l) = log else {
            return None;
        };
        // Inspect the most recent Phase step for this phase to determine entry reason and step index.
        let Some((idx, step)) = l.steps.iter().enumerate().rev().find(|(_i, s)| match s {
            react_core::session::ThreadStep::Phase { phase: p, .. } => p == phase.as_str(),
            _ => false,
        }) else {
            return None;
        };
        match step {
            react_core::session::ThreadStep::Phase { reason_code, .. } => {
                if reason_code.as_deref() == Some("review_actionable_true") {
                    Some(idx)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    async fn approve_cleanse_plan_draft_and_advance(
        thread_store: &ThreadStore,
        thread_id: &str,
        phase: control_flow::Phase,
        actx: &AgentCtx,
        log_len: usize,
        transition_reason_code: &str,
        transition_reason_detail: serde_json::Value,
    ) -> Result<bool, String> {
        let Some(mut p) = crate::data_engineer::plan::load_cleanse_plan(actx).await else {
            return Ok(false);
        };
        if p.status != crate::data_engineer::plan::PlanStatus::Draft {
            return Ok(false);
        }

        // Defensive grounding at approval time (facts can change; never assume).
        let mut candidates: Vec<String> = Vec::new();
        for t in p.tasks.iter() {
            if !t.dataset_id.trim().is_empty() {
                candidates.push(t.dataset_id.trim().to_string());
            }
        }
        for b in p.batches.iter() {
            for ds in b.iter() {
                if !ds.trim().is_empty() {
                    candidates.push(ds.trim().to_string());
                }
            }
        }
        candidates.sort();
        candidates.dedup();
        let grounded = crate::data_engineer::dataset_truth::build_grounded_raw_dataset_set(
            actx,
            &actx.warehouse,
            &candidates,
        )
        .await;
        crate::data_engineer::plan::prune_cleanse_plan_to_grounded_raw_datasets(
            &mut p,
            &grounded.allowed,
        );
        if p.tasks.is_empty() || p.batches.is_empty() {
            p.status = crate::data_engineer::plan::PlanStatus::Cancelled;
            let _ = crate::data_engineer::plan::save_cleanse_plan(actx, &p).await;
            // Stay in plan phase; the next iteration will generate a new plan.
            control_flow::append_phase_with_reason(
                thread_store,
                thread_id,
                Some("agent".to_string()),
                Some(phase),
                phase,
                Some("plan_pruned_empty"),
                Some(serde_json::json!({ "plan_key": p.plan_key })),
            )
            .await?;
            return Ok(true);
        }

        // Auto-heal (semantic): ensure the approved plan is executable (or cancel so we can replan).
        let v = crate::data_engineer::plan::ensure_cleanse_plan_semantically_valid_or_repaired(
            actx, &mut p,
        )
        .await?;
        if !v.ok {
            p.status = crate::data_engineer::plan::PlanStatus::Cancelled;
            let _ = crate::data_engineer::plan::save_cleanse_plan(actx, &p).await;
            control_flow::append_phase_with_reason(
                thread_store,
                thread_id,
                Some("agent".to_string()),
                Some(phase),
                phase,
                Some("plan_semantic_invalid"),
                Some(serde_json::json!({ "plan_key": p.plan_key, "errors": v.errors })),
            )
            .await?;
            return Ok(true);
        }

        p.status = crate::data_engineer::plan::PlanStatus::Approved;
        // Scope progress to *this* plan instance so old tool calls can't auto-complete a newly approved plan.
        p.progress.last_applied_step_idx = log_len;
        let _ = crate::data_engineer::plan::save_cleanse_plan(actx, &p).await;

        control_flow::append_phase_with_reason(
            thread_store,
            thread_id,
            Some("agent".to_string()),
            Some(phase),
            control_flow::Phase::CleanseAuthor,
            Some(transition_reason_code),
            Some(transition_reason_detail),
        )
        .await?;
        Ok(true)
    }

    async fn approve_model_plan_draft_and_advance(
        thread_store: &ThreadStore,
        thread_id: &str,
        phase: control_flow::Phase,
        actx: &AgentCtx,
        log_len: usize,
        transition_reason_code: &str,
        transition_reason_detail: serde_json::Value,
    ) -> Result<bool, String> {
        let Some(mut p) = crate::data_engineer::plan::load_model_plan(actx).await else {
            return Ok(false);
        };
        if p.status != crate::data_engineer::plan::PlanStatus::Draft {
            return Ok(false);
        }

        // Defensive grounding at approval time: gold must rely only on existing staging models.
        let stg =
            crate::data_engineer::dataset_truth::discover_staging_models_from_storage(actx).await;
        crate::data_engineer::plan::prune_model_plan_to_grounded_staging_models(
            &mut p,
            &stg.allowed_models,
        );
        if p.tasks.is_empty() || p.batches.is_empty() {
            p.status = crate::data_engineer::plan::PlanStatus::Cancelled;
            let _ = crate::data_engineer::plan::save_model_plan(actx, &p).await;
            // Stay in plan phase; the next iteration will generate a new plan.
            control_flow::append_phase_with_reason(
                thread_store,
                thread_id,
                Some("agent".to_string()),
                Some(phase),
                phase,
                Some("plan_pruned_empty"),
                Some(serde_json::json!({ "plan_key": p.plan_key })),
            )
            .await?;
            return Ok(true);
        }

        // Auto-heal (semantic): ensure the approved plan is executable (or cancel so we can replan).
        let v = crate::data_engineer::plan::ensure_model_plan_semantically_valid_or_repaired(
            actx,
            &mut p,
            &stg.allowed_models,
        )
        .await?;
        if !v.ok {
            p.status = crate::data_engineer::plan::PlanStatus::Cancelled;
            let _ = crate::data_engineer::plan::save_model_plan(actx, &p).await;
            control_flow::append_phase_with_reason(
                thread_store,
                thread_id,
                Some("agent".to_string()),
                Some(phase),
                phase,
                Some("plan_semantic_invalid"),
                Some(serde_json::json!({ "plan_key": p.plan_key, "errors": v.errors })),
            )
            .await?;
            return Ok(true);
        }

        p.status = crate::data_engineer::plan::PlanStatus::Approved;
        p.progress.last_applied_step_idx = log_len;
        let _ = crate::data_engineer::plan::save_model_plan(actx, &p).await;

        control_flow::append_phase_with_reason(
            thread_store,
            thread_id,
            Some("agent".to_string()),
            Some(phase),
            control_flow::Phase::ModelAuthor,
            Some(transition_reason_code),
            Some(transition_reason_detail),
        )
        .await?;
        Ok(true)
    }

    async fn repair_plan_json_payload_via_llm(
        thread_store: &ThreadStore,
        thread_id: &str,
        actx: &AgentCtx,
        phase: control_flow::Phase,
        expected_kind: &str,
        bad_payload: &serde_json::Value,
        err: &str,
        attempt: usize,
    ) -> Result<react_core::session::ThreadResult, String> {
        // Record an explicit guard step so the UI can surface "plan was invalid and is being repaired".
        let ts = chrono::Utc::now().to_rfc3339();
        let reason = format!(
            "Plan JSON failed validation (attempt {attempt}). expected_kind={expected_kind}. error={err}"
        );
        let step = react_core::session::ThreadStep::GuardBlock {
            phase: phase.as_str().to_string(),
            kind: "plan_json_invalid".to_string(),
            reason: reason.clone(),
            observation: react_core::session::Observation::fail(vec![reason.clone()]),
            ts: ts.clone(),
            agent: "agent".to_string(),
        };
        let _ = thread_store.append_step(thread_id, step.clone()).await;
        control_flow::append_phase_with_reason(
            thread_store,
            thread_id,
            Some("agent".to_string()),
            Some(phase),
            phase,
            Some("phase_blocked"),
            Some(serde_json::json!({
                "kind": "plan_json_invalid",
                "attempt": attempt,
                "expected_kind": expected_kind,
                "error": err,
            })),
        )
        .await?;

        // Build a repair-only prompt: include the FULL invalid payload and the parse error.
        let payload_str =
            serde_json::to_string_pretty(bad_payload).unwrap_or_else(|_| bad_payload.to_string());
        let q = format!(
            "Your previous plan JSON payload was invalid and could not be parsed by the server.\n\
You MUST fix it and re-emit the plan.\n\n\
Validation error:\n{err}\n\n\
Invalid payload JSON (FULL, do not omit content):\n{payload_str}\n\n\
Hard constraints:\n\
- The entire response must be STRICT JSON only.\n\
- Every checklist item's `evidence` must be an empty array `[]` (no strings, no objects).\n\n\
Now re-emit ONLY the corrected final envelope with kind=\"{expected_kind}\"."
        );

        let sys = crate::util::time_context::with_time_context(
            Self::plan_json_repair_system_prompt(expected_kind),
        );

        // No tools for repair: any tool call should fail and force a retry.
        let registry = ToolRegistry::new();
        let tools_card = "";
        let llm_options = Some(LlmCallOptions {
            // Strict JSON repair emitter: keep variance minimal.
            temperature: Some(0.0),
            top_p: Some(1.0),
            max_output_tokens: Some(1400),
        });
        match Agent::run_until_block(&registry, actx, &sys, tools_card, &q, llm_options).await {
            Ok(RunOutcome::Final {
                thread_id: _tid,
                result,
            }) => Ok(result),
            Ok(RunOutcome::AwaitUser {
                thread_id: _tid,
                prompt,
            }) => Err(format!(
                "plan json repair failed: model requested user input (not allowed). prompt={prompt}"
            )),
            Ok(RunOutcome::AwaitApproval {
                thread_id: _tid,
                prompt,
            }) => Err(format!(
                "plan json repair failed: model requested approval (not allowed). prompt={prompt}"
            )),
            Err(e) => Err(format!("plan json repair failed: {e}")),
        }
    }

    async fn authoring_complete_reason_detail(
        thread_store: &ThreadStore,
        thread_id: &str,
        has_proj: bool,
        has_models: bool,
    ) -> serde_json::Value {
        let log_now = thread_store.get(thread_id).await.ok();
        let guard = control_flow::derive_guard_state(log_now.as_ref());
        serde_json::json!({
            "invariants": {
                "has_dbt_project_yml": has_proj,
                "has_any_models": has_models,
            },
            "guard_state": {
                "last_validate_failed": guard.last_validate_failed,
                "mutated_since_fail": guard.mutated_since_fail,
                "patched_since_fail": guard.patched_since_fail,
                "mutation_failures_since_validate": guard.mutation_failures_since_validate,
                "probe_required": guard.probe_required,
                "probe_satisfied": guard.probe_satisfied,
            }
        })
    }
    fn phase_start_idx(
        log: &react_core::session::ThreadLog,
        phase: control_flow::Phase,
    ) -> Option<usize> {
        for (i, step) in log.steps.iter().enumerate().rev() {
            if let react_core::session::ThreadStep::Phase { phase: p, .. } = step {
                if p == phase.as_str() {
                    return Some(i);
                }
            }
        }
        None
    }

    /// Agent-mode hardening: allow at most one `ask_approval` per authoring phase.
    ///
    /// Rationale: once the user has approved the table set/plan for the phase, repeated approval prompts
    /// can trap the authoring loop. In agent mode we want the model to proceed to scaffolding.
    fn allow_ask_approval_in_phase(
        log: Option<&react_core::session::ThreadLog>,
        phase: control_flow::Phase,
    ) -> bool {
        let Some(log) = log else { return true };
        let Some(start) = Self::phase_start_idx(log, phase) else {
            return true;
        };
        // Monotonic: once approval/reject is seen within this phase, never allow ask_approval again
        // (even if later user messages are "continue", "ok", etc).
        for step in log.steps.iter().skip(start + 1) {
            let react_core::session::ThreadStep::User { text, .. } = step else {
                continue;
            };
            if Self::parse_user_decision(text).is_some() {
                return false;
            }
        }
        true
    }

    async fn has_any_gold_model_sql(actx: &AgentCtx) -> bool {
        let base = actx
            .keyspace
            .dbt_prefix(&actx.scope)
            .trim_end_matches('/')
            .to_string();
        let prefixes = [
            format!("{}/models/core/", base),
            format!("{}/models/marts/", base),
        ];
        for pref in prefixes.iter() {
            if let Ok(keys) = actx.storage.list_prefix(pref).await {
                for k in keys {
                    if !k.ends_with(".sql") {
                        continue;
                    }
                    if k.contains("/_versions/") {
                        continue;
                    }
                    return true;
                }
            }
        }
        false
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

    fn normalize_string_vec(xs: &[String]) -> Vec<String> {
        let mut out: Vec<String> = xs
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    fn same_opt_text(a: &Option<String>, b: &Option<String>) -> bool {
        let aa = a.as_deref().unwrap_or("").trim();
        let bb = b.as_deref().unwrap_or("").trim();
        aa == bb
    }

    fn plan_update_summary_cleanse(
        prev: Option<&crate::data_engineer::plan::CleansePlan>,
        next: &crate::data_engineer::plan::CleansePlan,
        review_entry_step_idx: Option<usize>,
    ) -> serde_json::Value {
        use crate::data_engineer::plan::ChecklistOrigin;
        let mut prev_by_id: std::collections::BTreeMap<
            String,
            &crate::data_engineer::plan::CleanseTask,
        > = std::collections::BTreeMap::new();
        if let Some(p) = prev {
            for t in p.tasks.iter() {
                prev_by_id.insert(t.dataset_id.clone(), t);
            }
        }
        let mut next_ids: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut added_tasks = 0usize;
        let mut removed_tasks = 0usize;
        let mut touched_tasks = 0usize;
        let mut review_items_total = 0usize;
        let mut top_items: Vec<serde_json::Value> = Vec::new();

        for t in next.tasks.iter() {
            next_ids.insert(t.dataset_id.clone());
            let prev_task = prev_by_id.get(&t.dataset_id).copied();
            let mut parts: Vec<String> = Vec::new();
            if prev_task.is_none() {
                added_tasks += 1;
                parts.push("new task".to_string());
            }
            if let Some(pt) = prev_task {
                if Self::normalize_string_vec(&pt.invariants) != Self::normalize_string_vec(&t.invariants) {
                    parts.push("invariants".to_string());
                }
                let mut prev_ci: std::collections::BTreeMap<
                    String,
                    &crate::data_engineer::plan::PlanChecklistItem,
                > = std::collections::BTreeMap::new();
                for it in pt.checklist.iter() {
                    prev_ci.insert(it.checklist_item_id.clone(), it);
                }
                let mut next_ci_ids: std::collections::BTreeSet<String> =
                    std::collections::BTreeSet::new();
                let mut added_ci: Vec<String> = Vec::new();
                let mut changed_ci: Vec<String> = Vec::new();
                for it in t.checklist.iter() {
                    next_ci_ids.insert(it.checklist_item_id.clone());
                    match prev_ci.get(&it.checklist_item_id) {
                        None => added_ci.push(it.checklist_item_id.clone()),
                        Some(prev_it) => {
                            if prev_it.label.trim() != it.label.trim()
                                || !Self::same_opt_text(&prev_it.details, &it.details)
                            {
                                changed_ci.push(it.checklist_item_id.clone());
                            }
                        }
                    }
                }
                let mut removed_ci: Vec<String> = Vec::new();
                for k in prev_ci.keys() {
                    if !next_ci_ids.contains(k) {
                        removed_ci.push(k.clone());
                    }
                }
                if !added_ci.is_empty() {
                    added_ci.sort();
                    parts.push(format!("+{}", added_ci.join(",")));
                }
                if !changed_ci.is_empty() {
                    changed_ci.sort();
                    parts.push(format!("~{}", changed_ci.join(",")));
                }
                if !removed_ci.is_empty() {
                    removed_ci.sort();
                    parts.push(format!("-{}", removed_ci.join(",")));
                }
            }

            let mut review_items: Vec<String> = t
                .checklist
                .iter()
                .filter(|it| it.origin == ChecklistOrigin::ReviewActionable)
                .filter(|it| {
                    if let Some(idx) = review_entry_step_idx {
                        it.origin_step_idx == Some(idx)
                    } else {
                        true
                    }
                })
                .map(|it| it.checklist_item_id.clone())
                .collect();
            review_items.sort();
            review_items.dedup();
            review_items_total += review_items.len();
            if !review_items.is_empty() {
                parts.push(format!("review:{}", review_items.join(",")));
            }

            if !parts.is_empty() {
                touched_tasks += 1;
                if top_items.len() < 5 {
                    top_items.push(serde_json::json!({
                        "task_id": t.dataset_id,
                        "summary": parts.join(", ")
                    }));
                }
            }
        }

        if let Some(p) = prev {
            for t in p.tasks.iter() {
                if !next_ids.contains(&t.dataset_id) {
                    removed_tasks += 1;
                }
            }
        }

        serde_json::json!({
            "kind": "cleanse",
            "from_plan_key": prev.map(|p| p.plan_key.clone()),
            "to_plan_key": next.plan_key,
            "counts": {
                "added_tasks": added_tasks,
                "removed_tasks": removed_tasks,
                "touched_tasks": touched_tasks,
                "review_items": review_items_total
            },
            "top_items": top_items
        })
    }

    fn plan_update_summary_model(
        prev: Option<&crate::data_engineer::plan::ModelPlan>,
        next: &crate::data_engineer::plan::ModelPlan,
        review_entry_step_idx: Option<usize>,
    ) -> serde_json::Value {
        use crate::data_engineer::plan::ChecklistOrigin;
        let mut prev_by_id: std::collections::BTreeMap<
            String,
            &crate::data_engineer::plan::ModelTask,
        > = std::collections::BTreeMap::new();
        if let Some(p) = prev {
            for t in p.tasks.iter() {
                prev_by_id.insert(t.name.clone(), t);
            }
        }
        let mut next_ids: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut added_tasks = 0usize;
        let mut removed_tasks = 0usize;
        let mut touched_tasks = 0usize;
        let mut review_items_total = 0usize;
        let mut top_items: Vec<serde_json::Value> = Vec::new();

        for t in next.tasks.iter() {
            next_ids.insert(t.name.clone());
            let prev_task = prev_by_id.get(&t.name).copied();
            let mut parts: Vec<String> = Vec::new();
            if prev_task.is_none() {
                added_tasks += 1;
                parts.push("new task".to_string());
            }
            if let Some(pt) = prev_task {
                if pt.goal.trim() != t.goal.trim() {
                    parts.push("goal".to_string());
                }
                if Self::normalize_string_vec(&pt.inputs) != Self::normalize_string_vec(&t.inputs) {
                    parts.push("inputs".to_string());
                }
                if Self::normalize_string_vec(&pt.invariants) != Self::normalize_string_vec(&t.invariants) {
                    parts.push("invariants".to_string());
                }
                let mut prev_ci: std::collections::BTreeMap<
                    String,
                    &crate::data_engineer::plan::PlanChecklistItem,
                > = std::collections::BTreeMap::new();
                for it in pt.checklist.iter() {
                    prev_ci.insert(it.checklist_item_id.clone(), it);
                }
                let mut next_ci_ids: std::collections::BTreeSet<String> =
                    std::collections::BTreeSet::new();
                let mut added_ci: Vec<String> = Vec::new();
                let mut changed_ci: Vec<String> = Vec::new();
                for it in t.checklist.iter() {
                    next_ci_ids.insert(it.checklist_item_id.clone());
                    match prev_ci.get(&it.checklist_item_id) {
                        None => added_ci.push(it.checklist_item_id.clone()),
                        Some(prev_it) => {
                            if prev_it.label.trim() != it.label.trim()
                                || !Self::same_opt_text(&prev_it.details, &it.details)
                            {
                                changed_ci.push(it.checklist_item_id.clone());
                            }
                        }
                    }
                }
                let mut removed_ci: Vec<String> = Vec::new();
                for k in prev_ci.keys() {
                    if !next_ci_ids.contains(k) {
                        removed_ci.push(k.clone());
                    }
                }
                if !added_ci.is_empty() {
                    added_ci.sort();
                    parts.push(format!("+{}", added_ci.join(",")));
                }
                if !changed_ci.is_empty() {
                    changed_ci.sort();
                    parts.push(format!("~{}", changed_ci.join(",")));
                }
                if !removed_ci.is_empty() {
                    removed_ci.sort();
                    parts.push(format!("-{}", removed_ci.join(",")));
                }
            }

            let mut review_items: Vec<String> = t
                .checklist
                .iter()
                .filter(|it| it.origin == ChecklistOrigin::ReviewActionable)
                .filter(|it| {
                    if let Some(idx) = review_entry_step_idx {
                        it.origin_step_idx == Some(idx)
                    } else {
                        true
                    }
                })
                .map(|it| it.checklist_item_id.clone())
                .collect();
            review_items.sort();
            review_items.dedup();
            review_items_total += review_items.len();
            if !review_items.is_empty() {
                parts.push(format!("review:{}", review_items.join(",")));
            }

            if !parts.is_empty() {
                touched_tasks += 1;
                if top_items.len() < 5 {
                    top_items.push(serde_json::json!({
                        "task_id": t.name,
                        "summary": parts.join(", ")
                    }));
                }
            }
        }

        if let Some(p) = prev {
            for t in p.tasks.iter() {
                if !next_ids.contains(&t.name) {
                    removed_tasks += 1;
                }
            }
        }

        serde_json::json!({
            "kind": "model",
            "from_plan_key": prev.map(|p| p.plan_key.clone()),
            "to_plan_key": next.plan_key,
            "counts": {
                "added_tasks": added_tasks,
                "removed_tasks": removed_tasks,
                "touched_tasks": touched_tasks,
                "review_items": review_items_total
            },
            "top_items": top_items
        })
    }

    fn is_mutation_step_for_review(step: &react_core::session::ThreadStep) -> bool {
        match step {
            react_core::session::ThreadStep::ArtifactSaved { .. } => true,
            react_core::session::ThreadStep::ToolEnd { name, args, .. } => match name.as_str() {
                "approve_and_save_artifact"
                | "approve_and_save_artifact_batch"
                | "staging_model" => true,
                "dbt_files" => args
                    .get("op")
                    .and_then(|v| v.as_str())
                    .map(|op| op == "put")
                    .unwrap_or(false),
                _ => false,
            },
            _ => false,
        }
    }

    fn compact_mutation_summary(step: &react_core::session::ThreadStep) -> serde_json::Value {
        let (action, ok, args_v) = match step {
            react_core::session::ThreadStep::ArtifactSaved {
                kind,
                name,
                dataset_id,
                key,
                status,
                lines_added,
                lines_removed,
                observation,
                ..
            } => (
                "artifact_saved".to_string(),
                observation.ok,
                serde_json::json!({
                    "kind": kind,
                    "name": name,
                    "dataset_id": dataset_id,
                    "key": key,
                    "status": status,
                    "lines_added": lines_added,
                    "lines_removed": lines_removed,
                }),
            ),
            react_core::session::ThreadStep::ToolEnd {
                name,
                args,
                observation,
                ..
            } => {
                let args_v = match name.as_str() {
                    "dbt_files" => serde_json::json!({
                        "op": args.get("op"),
                        "path": args.get("path"),
                    }),
                    "approve_and_save_artifact" => serde_json::json!({
                        "kind": args.get("kind"),
                        "name": args.get("name"),
                        "dataset_id": args.get("dataset_id"),
                    }),
                    "approve_and_save_artifact_batch" => {
                        let names: Vec<serde_json::Value> = args
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
                            "items_count": args.get("items").and_then(|v| v.as_array()).map(|a| a.len()),
                            "names_head": names,
                        })
                    }
                    "staging_model" => serde_json::json!({
                        "dataset_ids": args.get("dataset_ids"),
                        "written_keys": observation.extra.get("written_keys"),
                    }),
                    _ => args.clone(),
                };
                (name.clone(), observation.ok, args_v)
            }
            other => (
                "unknown".to_string(),
                true,
                serde_json::to_value(other).unwrap_or(serde_json::Value::Null),
            ),
        };

        serde_json::json!({
            "action": action,
            "ok": ok,
            "args": args_v,
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
        let entry = log.steps.iter().rev().find_map(|s| match s {
            react_core::session::ThreadStep::Phase {
                reason_code,
                reason_detail,
                ..
            } => Some((reason_code.as_deref(), reason_detail.as_ref())),
            _ => None,
        });
        let entry_reason_code: Option<&str> = entry.and_then(|(rc, _)| rc);
        let entry_reason_detail: serde_json::Value = entry
            .and_then(|(_, rd)| rd.cloned())
            .unwrap_or(serde_json::Value::Null);

        // Prior review context: last phase transition emitted from a review decision.
        let mut prior_review_block: Option<String> = None;
        let mut mutations_since: Vec<serde_json::Value> = Vec::new();

        if let Some((idx, step)) = log.steps.iter().enumerate().rev().find(|(_, s)| match s {
            react_core::session::ThreadStep::Phase {
                reason_code: Some(rc),
                ..
            } => rc == "review_actionable_true" || rc == "review_actionable_false",
            _ => false,
        }) {
            let rd = match step {
                react_core::session::ThreadStep::Phase { reason_detail, .. } => {
                    reason_detail.clone().unwrap_or(serde_json::Value::Null)
                }
                _ => serde_json::Value::Null,
            };
            let review_phase = rd
                .get("review_phase")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
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
             - Gold focus: author ONLY gold/core/mart models in this phase, and only SELECT from silver/staging models (use ref('stg_*')).\n\
             - Do NOT reference raw/bronze sources in gold model SQL.\n\
             - You MAY investigate raw/bronze via sql_schema/sql_sample/sql_stats/vect_query/run_sql to detect missing data, but treat raw as discovery only.\n\
             - If you find useful raw fields/tables missing in silver, request a silver expansion:\n\
               - Use ask_approval to list the missing tables/fields to add to silver.\n\
               - After approval, if apply_next_cleanse_batch is available (plan-batched mode), use it to execute the next approved silver batch deterministically.\n\
                 Otherwise, use staging_model (or dbt_files op=patch) to add them to silver BEFORE continuing gold.\n\
             - Search DBT examples (search_dbt_examples) and adopt conventions from the top match.\n\
             - Model relationships and flow:\n\
               - Identify join keys (user/profile/account/session/device identifiers) across the approved tables using sql_schema + sql_sample/sql_stats.\n\
               - Identify event time fields and ordering semantics; do NOT assume the timestamp column name.\n\
               - For event-style datasets, prefer building a core/funnel mart that sequences events per entity and computes step completion + step-to-step durations.\n\
              - Add dbt tests (not_null/unique/relationships) for the chosen keys and important timestamps.\n\
              - IMPORTANT: be data-aware and permissive: NEVER add unconditional not_null on parsed/cast timestamp fields produced via try_cast; instead use conditional tests with where: anchored on the raw value being present (and document why).\n\
             - Batch scaffolding: use dbt_files op=patch to write MANY files, but keep each call small enough to fit the output limit.\n\
               - If you need more files, do multiple dbt_files calls over multiple steps.\n\
             - IMPORTANT: use `dbt_files op=patch` for ALL DBT project files (e.g. path='dbt_project.yml', 'packages.yml', 'models/schema.yml', and model SQL).\n\
             - IMPORTANT (gold progress): you MUST author gold models under `models/marts/` or `models/core/` in this phase.\n\
               - Prefer apply_next_model_batch when available (plan-batched mode) so the suite executes the approved batch deterministically (do NOT supply items).\n\
               - Otherwise, prefer `gold_model` to write marts in batches of up to 5 models per call.\n\
               - Gold models MUST ONLY select from silver/staging via ref('stg_*') and MUST NOT use source().\n\
             - Staging/silver naming is STRICT and deterministic:\n\
               - Staging models MUST be written to: 'models/staging/stg_<source_schema>_<source_table>.sql' (single underscores, sanitized).\n\
               - Do NOT invent alternate naming schemes (no stg_<table>, no double-underscore variants). If a table already exists, you are updating it, not creating a new one.\n\
             - NOTE: In suite agent-mode runs, validation/publish may be handled by deterministic suite phases. Follow the current tool card; if dbt_validate/publish are not available, focus on authoring and let the suite validate/publish later.\n\
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
             - Prefer apply_next_cleanse_batch when available (plan-batched mode) so the suite executes the approved batch deterministically (do NOT supply dataset_ids).\n\
               Otherwise, use `staging_model` to author/update staging models (cleansing + nested field extraction). Treat user instructions as authoritative constraints.\n\
             - Silver tier must land in the configured Athena silver database.\n\
             - Do NOT assume table names.\n\
             - If embeddings/vect search yields no candidates, call sql_schema with no args to list tables.\n\
             - Hybrid selection: propose a shortlist of tables (with brief reasons based on schema/stats), then ask_approval to confirm the table list before writing any artifacts.\n\
             - Coverage requirement: include ALL raw/bronze tables in scope by default, and include all valid fields from those tables in silver.\n\
               - Do NOT drop columns; preserve raw values (e.g., *_raw) and add cleaned/cast columns alongside them.\n\
               - If a field is unusable, keep the raw column and add a best-effort cleaned column with safe casting/normalization.\n\
             - Row preservation (CRITICAL): silver/staging is a row-preserving cleanse layer.\n\
               - Do NOT filter rows, deduplicate, or enforce grains/primary keys in silver.\n\
               - If raw/bronze values are NULL/blank, it is valid for silver to produce NULL/blank after cleansing.\n\
               - Prefer quality flags (has_*, is_valid_*) and conditional tests instead of row drops.\n\
             - Model relationships and flow:\n\
               - Identify join keys (user/profile/account/session/device identifiers) and timestamp fields using sql_schema + sql_sample/sql_stats.\n\
               - Prefer staged normalization (consistent key/timestamp names) to make downstream joins reliable.\n\
              - Add dbt tests for chosen keys and key timestamps (data-aware):\n\
                - Prefer conditional tests with where: anchored on raw input presence.\n\
                - Avoid unique tests in silver unless the raw source is proven unique and you intend to enforce it here.\n\
              - IMPORTANT: be data-aware and permissive: NEVER add unconditional not_null on parsed/cast timestamp fields produced via try_cast; instead use conditional tests with where: anchored on the raw value being present (and document why).\n\
             - Batch scaffolding: use dbt_files op=patch to write MANY files, but keep each call small enough to fit the output limit.\n\
               - If you need more files, do multiple dbt_files calls over multiple steps.\n\
             - Use `dbt_files op=patch` for ALL DBT project files (dbt_project.yml, packages.yml, models/schema.yml, and staging SQL).\n\
             - Staging/silver naming is STRICT and deterministic:\n\
               - Staging models MUST be written to: 'models/staging/stg_<source_schema>_<source_table>.sql' (single underscores, sanitized).\n\
               - Do NOT invent alternate naming schemes (no stg_<table>, no double-underscore variants). If a table already exists, you are updating it, not creating a new one.\n\
             - After saving artifacts: ALWAYS validate with dbt_validate. If validate fails, iterate (edit artifacts, re-validate) until clean.\n\
             - When validation is clean: call publish_dbt_to_provider (views by default; propose tables/incremental with rationale and await approval).\n\
             - Use catalog_note to record notable cleansing decisions and assumptions (preview if material).",
            question
        )
    }

    fn build_tools(agent_type: &str, sctx: &SuiteCtx) -> Result<ToolRegistry, String> {
        use crate::data_engineer::tools::{
            artifacts::ArtifactsTool, dbt_files::DbtFilesTool, sql_run::SqlRunTool,
            sql_sample::SqlSampleTool, sql_schema::SqlSchemaTool, sql_stats::SqlStatsTool,
            vect_query::VectQueryTool,
        };

        /// Thread-derived guard: blocks repeated dbt_validate after failure until a mutation occurs,
        /// and enforces a data probe after runtime (build/run) failures.
        struct ThreadDerivedDbtValidateTool {
            inner: tools::dbt_validate::DbtValidateTool,
        }
        #[async_trait::async_trait]
        impl react_core::tools::Tool for ThreadDerivedDbtValidateTool {
            fn name(&self) -> &'static str {
                "dbt_validate"
            }
            async fn call(
                &self,
                args: serde_json::Value,
                ctx: &react_core::agent::AgentCtx,
            ) -> Result<serde_json::Value, String> {
                let build = args.get("build").and_then(|v| v.as_bool()).unwrap_or(false);
                let run = args.get("run").and_then(|v| v.as_bool()).unwrap_or(false);
                let runtime_validate = build || run;
                if let (Some(store), Some(tid)) =
                    (ctx.thread_store.as_ref(), ctx.thread_id.as_deref())
                {
                    let log = store.get(tid).await.ok();
                    let guard =
                        crate::data_engineer::control_flow::derive_guard_state(log.as_ref());
                    if guard.last_validate_failed && !guard.mutated_since_fail {
                        return Err(
                            "dbt_validate is blocked after a failed validation until you APPLY A FIX to the dbt project.\n\
                             Next step must be a mutating fix action (e.g. `staging_model` or `dbt_files op=patch` to update schema/tests)."
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
        registry.register(SqlSampleTool {
            query: query.clone(),
        });
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
                    fn name(&self) -> &'static str {
                        "dbt_files"
                    }
                    async fn call(
                        &self,
                        args: serde_json::Value,
                        ctx: &react_core::agent::AgentCtx,
                    ) -> Result<serde_json::Value, String> {
                        let op = args.get("op").and_then(|x| x.as_str()).unwrap_or("get");
                        if op == "patch" {
                            return Err(
                                "dbt_files is read-only for review; use op='get' or op='list'"
                                    .to_string(),
                            );
                        }
                        self.inner.call(args, ctx).await
                    }
                }
                registry.register(ReadOnlyDbtFilesTool {
                    inner: DbtFilesTool {
                        datasets: sctx.datasets.clone(),
                    },
                });
                registry.register(ArtifactsTool);
            }
            // cleanse uses shared tools + authoring/validation/publish loop
            "cleanse" => {
                registry.register(SqlRunTool {
                    query: query.clone(),
                });
                registry.register(tools::ask_user::AskUserTool);
                registry.register(tools::ask_approval::AskApprovalTool);
                registry.register(tools::dbt_examples::SearchDbtExamplesTool);
                registry.register(tools::staging_model::StagingModelTool {
                    datasets: sctx.datasets.clone(),
                });
                registry.register(ThreadDerivedDbtValidateTool {
                    inner: tools::dbt_validate::DbtValidateTool {
                        datasets: sctx.datasets.clone(),
                        catalog: sctx.catalog.clone(),
                    },
                });
                registry.register(tools::publish_dbt_to_provider::PublishDbtToProviderTool {
                    datasets: sctx.datasets.clone(),
                    catalog: sctx.catalog.clone(),
                });
                registry.register(tools::sql_register::SqlRegisterTool);
                registry.register(tools::catalog_note::CatalogNoteTool);
                registry.register(DbtFilesTool {
                    datasets: sctx.datasets.clone(),
                });
                registry.register(ArtifactsTool);
            }
            // ask uses shared tools + user/approval interrupts + artifacts
            "ask" => {
                registry.register(SqlRunTool {
                    query: query.clone(),
                });
                registry.register(tools::ask_user::AskUserTool);
                registry.register(tools::ask_approval::AskApprovalTool);
                registry.register(DbtFilesTool {
                    datasets: sctx.datasets.clone(),
                });
                registry.register(ArtifactsTool);
            }
            // model uses ask tools + artifact authoring + dbt helpers + artifacts
            _ => {
                registry.register(SqlRunTool {
                    query: query.clone(),
                });
                registry.register(tools::ask_user::AskUserTool);
                registry.register(tools::ask_approval::AskApprovalTool);
                registry.register(tools::dbt_examples::SearchDbtExamplesTool);
                registry.register(tools::staging_model::StagingModelTool {
                    datasets: sctx.datasets.clone(),
                });
                registry.register(tools::gold_model::GoldModelTool);
                registry.register(ThreadDerivedDbtValidateTool {
                    inner: tools::dbt_validate::DbtValidateTool {
                        datasets: sctx.datasets.clone(),
                        catalog: sctx.catalog.clone(),
                    },
                });
                registry.register(tools::publish_dbt_to_provider::PublishDbtToProviderTool {
                    datasets: sctx.datasets.clone(),
                    catalog: sctx.catalog.clone(),
                });
                registry.register(tools::sql_register::SqlRegisterTool);
                registry.register(tools::catalog_note::CatalogNoteTool);
                registry.register(DbtFilesTool {
                    datasets: sctx.datasets.clone(),
                });
                registry.register(ArtifactsTool);
            }
        }

        Ok(registry)
    }

    fn build_tools_for_phase(
        phase: control_flow::Phase,
        guard: &control_flow::DerivedGuardState,
        _allow_ask_approval: bool,
        sctx: &SuiteCtx,
        allowed_batch: Option<AllowedBatch>,
    ) -> Result<(ToolRegistry, String), String> {
        use crate::data_engineer::tools::{
            artifacts::ArtifactsTool, dbt_files::DbtFilesTool, sql_run::SqlRunTool,
            sql_sample::SqlSampleTool, sql_schema::SqlSchemaTool, sql_stats::SqlStatsTool,
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
        reg.register(SqlSampleTool {
            query: query.clone(),
        });
        reg.register(VectQueryTool);
        reg.register(ArtifactsTool);

        let tools_card_lines: Vec<&'static str>;

        match phase {
            control_flow::Phase::CleansePlan | control_flow::Phase::ModelPlan => {
                // Plan phases: read-only discovery + (optional) probes. No dbt file mutations.
                reg.register(tools::ask_user::AskUserTool);
                reg.register(SqlRunTool {
                    query: query.clone(),
                });
                reg.register(tools::dbt_examples::SearchDbtExamplesTool);

                // Read-only dbt_files (no patch).
                struct ReadOnlyDbtFilesTool {
                    inner: DbtFilesTool,
                }
                #[async_trait::async_trait]
                impl react_core::tools::Tool for ReadOnlyDbtFilesTool {
                    fn name(&self) -> &'static str {
                        "dbt_files"
                    }
                    async fn call(
                        &self,
                        args: serde_json::Value,
                        ctx: &react_core::agent::AgentCtx,
                    ) -> Result<serde_json::Value, String> {
                        let op = args.get("op").and_then(|x| x.as_str()).unwrap_or("get");
                        if op == "patch" {
                            return Err(
                                "dbt_files is read-only in plan phases; use op='get' or op='list'"
                                    .to_string(),
                            );
                        }
                        self.inner.call(args, ctx).await
                    }
                }
                reg.register(ReadOnlyDbtFilesTool {
                    inner: DbtFilesTool {
                        datasets: sctx.datasets.clone(),
                    },
                });

                tools_card_lines = vec![
                    "Allowed tools (plan phase, read-only):",
                    "- dbt_files(args:{op:\"list\"|\"get\"|\"get_json\"|\"manifest_find\", path?:string, prefix?:string, pointer?:string, limit?:int, max_chars?:int})",
                    "  - IMPORTANT: use args.op (NOT args.type). For list use args.prefix (NOT path:\".\").",
                    "- sql_schema / sql_stats / sql_sample / vect_query (discovery context)",
                    "- run_sql (targeted probes)",
                    "- artifacts",
                    "- ask_user",
                    "",
                    "Not available: staging_model, gold_model, dbt_files patch, dbt_validate, publish_dbt_to_provider.",
                ];
            }
            control_flow::Phase::CleanseAuthor | control_flow::Phase::ModelAuthor => {
                // Authoring phases: allow investigation + mutations; validation/publish are suite-driven.
                //
                // If the last validation failed and no mutation has happened since, enforce a hard tool lock:
                // the next step MUST be a mutation.
                let hard_mutation_only = guard.last_validate_failed && !guard.mutated_since_fail;

                reg.register(tools::ask_user::AskUserTool);

                if hard_mutation_only {
                    // Patch-only dbt_files to avoid "read-only thrash" when we require a mutation next.
                    struct PutOnlyDbtFilesTool {
                        inner: DbtFilesTool,
                    }
                    #[async_trait::async_trait]
                    impl react_core::tools::Tool for PutOnlyDbtFilesTool {
                        fn name(&self) -> &'static str {
                            "dbt_files"
                        }
                        async fn call(
                            &self,
                            args: serde_json::Value,
                            ctx: &react_core::agent::AgentCtx,
                        ) -> Result<serde_json::Value, String> {
                            let op = args.get("op").and_then(|x| x.as_str()).unwrap_or("get");
                            if op != "patch" {
                                return Err("dbt_files is patch-only right now (a mutating fix is required before any further validation).".to_string());
                            }
                            self.inner.call(args, ctx).await
                        }
                    }
                    reg.register(PutOnlyDbtFilesTool {
                        inner: DbtFilesTool {
                            datasets: sctx.datasets.clone(),
                        },
                    });

                    // Keep targeted probes available: probe requirements can be asserted after runtime failures,
                    // and those probes must be satisfiable even when the next step must be a mutation.
                    reg.register(SqlRunTool {
                        query: query.clone(),
                    });

                    // Even in hard_mutation_only, schema batch tools are safe to expose because they are
                    // inherently mutating and can resolve common "YAML contract" failures without manual
                    // dbt_files patching.
                    let mut tool_lines: Vec<&'static str> = vec![
                        "Allowed tools (authoring phase; HARD constraint: mutation required next):",
                    ];
                    if phase == control_flow::Phase::CleanseAuthor {
                        reg.register(tools::apply_next_schema_batch::ApplyNextCleanseSchemaBatchTool {
                            datasets: sctx.datasets.clone(),
                        });
                        tool_lines.push("- apply_next_cleanse_schema_batch(args:{instructions?:string})");
                    }
                    if phase == control_flow::Phase::ModelAuthor {
                        reg.register(tools::apply_next_schema_batch::ApplyNextModelSchemaBatchTool {
                            datasets: sctx.datasets.clone(),
                        });
                        tool_lines.push("- apply_next_model_schema_batch(args:{instructions?:string})");
                    }
                    tool_lines.extend_from_slice(&[
                        "- dbt_files(args:{op:\"patch\", replace_file?:{path:string,new_text:string,expected_sha256?:string}|[{...}], replace_range?:{path:string,start_line:int,end_line:int,new_text:string,expected_sha256?:string}|[{...}], replace_list?:{path:string,edits:[{start_line:int,end_line:int,new_text:string}],expected_sha256?:string}|[{...}], path?:string})",
                        "- run_sql(args:{sql:string}) (targeted probes; required after runtime failures)",
                        "- ask_user(args:{prompt:string})",
                        "",
                        "Not available: read/explore tools, dbt_validate, publish_dbt_to_provider.",
                    ]);
                    tools_card_lines = tool_lines;
                } else {
                    // Normal authoring: allow read/explore + probes.
                    if phase == control_flow::Phase::CleanseAuthor {
                        if let Some(AllowedBatch::CleanseDatasetIds(allowed)) =
                            allowed_batch.clone()
                        {
                            // Deterministic plan-batched execution: LLM MUST NOT supply dataset_ids.
                            // The tool derives the exact next approved batch from the persisted plan.
                            let _ = allowed; // used only as an enablement signal
                            reg.register(tools::apply_next_batch::ApplyNextCleanseBatchTool {
                                datasets: sctx.datasets.clone(),
                            });
                            reg.register(tools::apply_next_schema_batch::ApplyNextCleanseSchemaBatchTool {
                                datasets: sctx.datasets.clone(),
                            });
                        } else {
                            reg.register(tools::staging_model::StagingModelTool {
                                datasets: sctx.datasets.clone(),
                            });
                            reg.register(tools::apply_next_schema_batch::ApplyNextCleanseSchemaBatchTool {
                                datasets: sctx.datasets.clone(),
                            });
                        }
                    }
                    if phase == control_flow::Phase::ModelAuthor {
                        if let Some(AllowedBatch::ModelItemNames(allowed)) = allowed_batch.clone() {
                            // Deterministic plan-batched execution: LLM MUST NOT supply items.
                            // The tool derives the exact next approved batch from the persisted plan.
                            let _ = allowed; // used only as an enablement signal
                            reg.register(tools::apply_next_batch::ApplyNextModelBatchTool);
                            reg.register(tools::apply_next_schema_batch::ApplyNextModelSchemaBatchTool {
                                datasets: sctx.datasets.clone(),
                            });
                        } else {
                            reg.register(tools::gold_model::GoldModelTool);
                            reg.register(tools::apply_next_schema_batch::ApplyNextModelSchemaBatchTool {
                                datasets: sctx.datasets.clone(),
                            });
                        }
                    }
                    reg.register(SqlRunTool {
                        query: query.clone(),
                    });
                    reg.register(tools::dbt_examples::SearchDbtExamplesTool);
                    reg.register(DbtFilesTool {
                        datasets: sctx.datasets.clone(),
                    });

                    let plan_batched_cleanse = phase == control_flow::Phase::CleanseAuthor
                        && matches!(allowed_batch, Some(AllowedBatch::CleanseDatasetIds(_)));
                    let plan_batched_model = phase == control_flow::Phase::ModelAuthor
                        && matches!(allowed_batch, Some(AllowedBatch::ModelItemNames(_)));
                    if plan_batched_cleanse {
                        tools_card_lines = vec![
							"Allowed tools (authoring phase; plan-batched, deterministic):",
							"- apply_next_cleanse_batch(args:{instructions?:string})",
							"- apply_next_cleanse_schema_batch(args:{instructions?:string})",
							"- dbt_files(args:{op:\"list\"|\"get\"|\"get_json\"|\"manifest_find\"|\"patch\", path?:string, prefix?:string, replace_file?:any, replace_range?:any, replace_list?:any, limit?:int, max_chars?:int})",
							"- sql_schema / sql_stats / sql_sample / vect_query (discovery context)",
							"- run_sql (targeted probes)",
							"- ask_user",
							"- artifacts",
							"",
							"Not available in this phase: staging_model (batch tool calls it deterministically), dbt_validate, publish_dbt_to_provider.",
						];
                    } else if plan_batched_model {
                        tools_card_lines = vec![
							"Allowed tools (authoring phase; plan-batched, deterministic):",
							"- apply_next_model_batch(args:{instructions?:string})",
							"- apply_next_model_schema_batch(args:{instructions?:string})",
							"- dbt_files(args:{op:\"list\"|\"get\"|\"get_json\"|\"manifest_find\"|\"patch\", path?:string, prefix?:string, replace_file?:any, replace_range?:any, replace_list?:any, limit?:int, max_chars?:int})",
							"- sql_schema / sql_stats / sql_sample / vect_query (discovery context)",
							"- run_sql (targeted probes)",
							"- ask_user",
							"- artifacts",
							"",
							"Not available in this phase: gold_model (batch tool calls it deterministically), dbt_validate, publish_dbt_to_provider.",
						];
                    } else {
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
							"- gold_model(args:{items:[{name:string, folder?:\"marts\"|\"core\", goal?:string, description?:string, inputs:[string], instructions?:string}]})",
							"  - IMPORTANT: max 5 items per call. Gold MUST use ref('stg_*') only; NO source().",
							"- dbt_files(args:{op:\"list\"|\"get\"|\"get_json\"|\"manifest_find\"|\"patch\", path?:string, prefix?:string, replace_file?:any, replace_range?:any, replace_list?:any, limit?:int, max_chars?:int})",
							"- ask_user(args:{prompt:string})",
							"",
							"Not available in this phase: dbt_validate, publish_dbt_to_provider (suite handles these deterministically).",
						];
                    }
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
                    fn name(&self) -> &'static str {
                        "dbt_files"
                    }
                    async fn call(
                        &self,
                        args: serde_json::Value,
                        ctx: &react_core::agent::AgentCtx,
                    ) -> Result<serde_json::Value, String> {
                        let op = args.get("op").and_then(|x| x.as_str()).unwrap_or("get");
                        if op == "patch" {
                            return Err("dbt_files is read-only in review phases; use op='get' or op='list'".to_string());
                        }
                        self.inner.call(args, ctx).await
                    }
                }
                reg.register(ReadOnlyDbtFilesTool {
                    inner: DbtFilesTool {
                        datasets: sctx.datasets.clone(),
                    },
                });

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
                tools_card_lines =
                    vec!["Allowed tools: (suite deterministic step; no agent tools)"];
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
            // Also detect whether the global semantic context exists.
            let global_key = sctx.keyspace.semantic_key(
                &sctx.scope,
                react_core::providers::catalog::types::GLOBAL_SEMANTIC_DATASET_ID,
            );
            let has_global = sctx.storage.get_json(&global_key).await.is_ok();

            for ds in dss.iter().take(5) {
                let id = ds.fqn();
                if let Ok(Some(_)) = cat.read_catalog(&sctx.scope, &id).await {
                    has_any = true;
                    break;
                }
            }
            if !has_any || !has_global {
                tracing::info!("data_engineer: no existing catalog found; building catalogs/stats for all datasets");
                let empty: HashMap<String, react_core::discover::Metadata> = HashMap::new();
                if !has_any {
                    if let Err(e) = cat
                        .build_all_with_progress(&sctx.scope, datasets.as_ref(), &empty, None)
                        .await
                    {
                        tracing::warn!("data_engineer: catalog bootstrap failed: {}", e);
                    }
                }

                // Best-effort LLM enrichment (dataset descriptions + global context).
                // Use a stable dataset_id map so the provider can chunk deterministically.
                let mut all: HashMap<String, react_core::discover::Metadata> = HashMap::new();
                for ds in dss.iter() {
                    all.insert(ds.fqn(), react_core::discover::Metadata::default());
                }
                if let Err(e) = cat.run_llm_enrichment_all(&sctx.scope, &all).await {
                    tracing::warn!("data_engineer: catalog enrichment failed: {}", e);
                }
            }
        }
    }

    async fn manifest_targeting_lines(
        actx: &AgentCtx,
        runtime_failures: &[serde_json::Value],
    ) -> Vec<String> {
        // Best-effort: map failing test(s) -> model file path(s) using target/manifest.json.
        let base = actx
            .keyspace
            .dbt_prefix(&actx.scope)
            .trim_end_matches('/')
            .to_string();
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
            let Some(name) = rf.get("name").and_then(|x| x.as_str()) else {
                continue;
            };

            // Find the manifest node for this test.
            let mut test_node: Option<(&String, &serde_json::Value)> = None;
            for (k, node) in nodes.iter() {
                let rt = node
                    .get("resource_type")
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                if rt != "test" {
                    continue;
                }
                let n = node.get("name").and_then(|x| x.as_str()).unwrap_or("");
                if n == name || k.ends_with(name) {
                    test_node = Some((k, node));
                    break;
                }
            }
            let Some((_test_id, test_node)) = test_node else {
                continue;
            };
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
                if model_file.is_empty() {
                    "(unknown)"
                } else {
                    model_file
                },
                if test_file.is_empty() {
                    "(unknown)"
                } else {
                    test_file
                }
            ));
        }
        out
    }

    async fn run_ask(
        thread_id: &str,
        question: &str,
        sctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String> {
        let sys = crate::util::time_context::with_time_context(prompts::ask_system_prompt());
        let tools_card = prompts::ask_tool_card();

        let pf = crate::preflight::CatalogPreflightProvider {
            discovery_limits: crate::preflight::discovery::DiscoveryLimits::default(),
            run_preflight_on_bundle: false,
        };
        let bundle = pf.run(thread_id, question, "ask", sctx).await.discovery;

        let registry = Self::build_tools("ask", sctx)?;
        let thread_store = ThreadStore::new(
            sctx.storage.clone(),
            sctx.scope.clone(),
            sctx.keyspace.clone(),
        );

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
            warehouse: sctx.warehouse.clone(),
            dbt: sctx.dbt.clone(),
            vector: sctx.vector.clone(),
            thread_store: Some(thread_store),
            exec_ctx: None,
            runtime: sctx
                .resolved_config
                .clone()
                .map(|c| c as Arc<dyn std::any::Any + Send + Sync>),
        };

        match Agent::run_until_block(&registry, &actx, &sys, &tools_card, question, None).await {
            Ok(RunOutcome::Final {
                thread_id: _tid,
                result,
            }) => Ok(vec![FlowFrame::Final {
                kind: result.kind,
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

    async fn run_review(
        thread_id: &str,
        question: &str,
        sctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String> {
        Self::ensure_catalog_bootstrap(sctx).await;
        let sys = crate::util::time_context::with_time_context(prompts::review_system_prompt());
        let tools_card = prompts::review_tool_card();

        let registry = Self::build_tools("review", sctx)?;
        let thread_store = ThreadStore::new(
            sctx.storage.clone(),
            sctx.scope.clone(),
            sctx.keyspace.clone(),
        );

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
            warehouse: sctx.warehouse.clone(),
            dbt: sctx.dbt.clone(),
            vector: sctx.vector.clone(),
            thread_store: Some(thread_store),
            exec_ctx: None,
            runtime: sctx
                .resolved_config
                .clone()
                .map(|c| c as Arc<dyn std::any::Any + Send + Sync>),
        };

        let prompt = Self::inject_review_question(question);
        match Agent::run_until_block(&registry, &actx, &sys, &tools_card, &prompt, None).await {
            Ok(RunOutcome::Final {
                thread_id: _tid,
                result,
            }) => Ok(vec![FlowFrame::Final {
                kind: result.kind,
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

    fn agent_tool_ctx(thread_id: &str, sctx: &SuiteCtx) -> AgentCtx {
        let thread_store = ThreadStore::new(
            sctx.storage.clone(),
            sctx.scope.clone(),
            sctx.keyspace.clone(),
        );
        AgentCtx {
            top_k: 30,
            per_step_timeout_secs: 10,
            max_steps: 6,
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
            warehouse: sctx.warehouse.clone(),
            dbt: sctx.dbt.clone(),
            vector: sctx.vector.clone(),
            thread_store: Some(thread_store),
            exec_ctx: None,
            runtime: sctx
                .resolved_config
                .clone()
                .map(|c| c as Arc<dyn std::any::Any + Send + Sync>),
        }
    }

    fn plan_agent_ctx(thread_id: &str, sctx: &SuiteCtx) -> AgentCtx {
        // Plan phases (cleanse_plan/model_plan) are tool-heavy: they must do discovery and evidence,
        // then emit a final plan object. The small 6-step budget used for some helper contexts can
        // cause a fallback Final ("No result") which then fails plan JSON parsing and shows up as a
        // misleading "final.kind/payload must be valid for the plan" error.
        let thread_store = ThreadStore::new(
            sctx.storage.clone(),
            sctx.scope.clone(),
            sctx.keyspace.clone(),
        );
        AgentCtx {
            top_k: 30,
            per_step_timeout_secs: 20,
            max_steps: 40,
            thread_id: Some(thread_id.to_string()),
            progress_tx: None,
            pre_step_tx: None,
            trace_tx: sctx.trace_tx.clone(),
            // Keep a single agent label for agent-mode runs; phase selection is handled by the outer loop.
            agent_name: Some("agent".to_string()),
            // Preserve strict interrupts (ask_user/ask_approval), but otherwise accept finals.
            policy: std::sync::Arc::new(InterruptOnlyPolicy),
            llm: sctx.llm.clone(),
            storage: sctx.storage.clone(),
            scope: sctx.scope.clone(),
            keyspace: sctx.keyspace.clone(),
            query: sctx.query.clone(),
            warehouse: sctx.warehouse.clone(),
            dbt: sctx.dbt.clone(),
            vector: sctx.vector.clone(),
            thread_store: Some(thread_store),
            exec_ctx: None,
            runtime: sctx
                .resolved_config
                .clone()
                .map(|c| c as Arc<dyn std::any::Any + Send + Sync>),
        }
    }

    async fn run_agent(
        thread_id: &str,
        question: &str,
        sctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String> {
        use control_flow::{DerivedGuardState, Phase};
        Self::ensure_catalog_bootstrap(sctx).await;

        // Phase-step budget is reset when we make clear forward progress (phase advances).
        // This prevents aborting a healthy thread that is steadily moving through phases,
        // while still bounding degenerate loops.
        let max_phase_steps: usize = std::env::var("AGENT_MAX_PHASE_STEPS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(60)
            .max(12)
            .min(400);

        let phase_index = |p: Phase| -> usize {
            match p {
                Phase::Preflight => 0,
                Phase::CleansePlan => 1,
                Phase::CleanseAuthor => 2,
                Phase::CleanseValidate => 3,
                Phase::CleanseReview => 4,
                Phase::ModelPlan => 5,
                Phase::ModelAuthor => 6,
                Phase::ModelValidate => 7,
                Phase::ModelReview => 8,
                Phase::PublishAwaitApproval => 9,
                Phase::Publish => 10,
                Phase::PostPublishReview => 11,
                Phase::Done => 12,
            }
        };

        let thread_store = ThreadStore::new(
            sctx.storage.clone(),
            sctx.scope.clone(),
            sctx.keyspace.clone(),
        );

        let mut out_frames: Vec<FlowFrame> = Vec::new();

        let mut remaining_steps = max_phase_steps;
        let mut total_steps: usize = 0;
        let mut max_phase_idx_seen: usize = 0;

        while remaining_steps > 0 {
            total_steps += 1;
            remaining_steps = remaining_steps.saturating_sub(1);

            let log = thread_store.get(thread_id).await.ok();
            let phase = control_flow::phase_from_log(log.as_ref());
            let idx = phase_index(phase);
            if idx > max_phase_idx_seen {
                max_phase_idx_seen = idx;
                // Reset the budget when we advance phases (i.e. not looping).
                remaining_steps = max_phase_steps;
            }
            let guard: DerivedGuardState = control_flow::derive_guard_state(log.as_ref());
            let allow_ask_approval = match phase {
                // Plan phases may require multiple approval prompts across iterations (reject -> revise -> ask again).
                Phase::CleansePlan | Phase::ModelPlan => true,
                _ => Self::allow_ask_approval_in_phase(log.as_ref(), phase),
            };

            // Helper: most recent dbt_validate error context (for prompt grounding).
            let mut last_validate_brief: Option<String> = None;
            let mut last_validate_failed_models: Vec<serde_json::Value> = Vec::new();
            if let Some(ref l) = log {
                for step in l.steps.iter().rev() {
                    let react_core::session::ThreadStep::ToolEnd {
                        name, observation, ..
                    } = step
                    else {
                        continue;
                    };
                    if name != "dbt_validate" {
                        continue;
                    }
                    // IMPORTANT: do not truncate dbt failures to a brief. Preserve full error output when it fits,
                    // otherwise include a deterministic excerpt (keyword-window matches).
                    if !observation.ok {
                        let max_chars = react_core::error_context::estimate_max_prompt_chars(
                            &Self::plan_agent_ctx(thread_id, sctx),
                        );
                        last_validate_brief =
                            Some(react_core::error_context::render_failure_context(
                                observation,
                                max_chars,
                            ));
                    }
                    // Best-effort: extract failing model(s) from dbt stdout so the authoring LLM
                    // can target a specific file even when plan batches are already complete.
                    let logs_v = observation
                        .extra
                        .get("logs")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    last_validate_failed_models =
                        dbt_error::extract_failed_models_from_logs(&logs_v);
                    break;
                }
            }

            match phase {
                Phase::Preflight => {
                    // Require core providers.
                    if sctx.query.is_none() {
                        return Ok(vec![FlowFrame::AwaitUser {
                            prompt: "Data Engineer agent requires a warehouse provider configured. Configure providers.warehouse and restart.".to_string(),
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
                        Phase::CleansePlan,
                        Some("preflight_ok"),
                        Some(serde_json::json!({
                            "dbt_project_key": key,
                            "has_query_provider": sctx.query.is_some(),
                            "has_dbt_provider": sctx.dbt.is_some(),
                        })),
                    )
                    .await?;
                    continue;
                }

                Phase::CleansePlan | Phase::ModelPlan => {
                    // Plan phases are read-only discovery + plan authoring. They persist an approved
                    // plan to storage and then drive the subsequent authoring phase deterministically.
                    let is_cleanse = phase == Phase::CleansePlan;
                    let actx = Self::plan_agent_ctx(thread_id, sctx);
                    let actionable_review_entry_step_idx =
                        Self::actionable_review_entry_step_idx(log.as_ref(), phase);
                    let entered_from_actionable_review =
                        actionable_review_entry_step_idx.is_some();
                    let prior_cleanse_plan_for_update = if entered_from_actionable_review && is_cleanse
                    {
                        crate::data_engineer::plan::load_cleanse_plan_any(&actx).await
                    } else {
                        None
                    };
                    let prior_model_plan_for_update = if entered_from_actionable_review && !is_cleanse
                    {
                        crate::data_engineer::plan::load_model_plan_any(&actx).await
                    } else {
                        None
                    };

                    // If the user already approved an existing draft, mark it approved and proceed.
                    if let Some(ref l) = log {
                        if let Some(start) = Self::phase_start_idx(l, phase) {
                            if let Some(last_user) =
                                l.steps.iter().skip(start + 1).rev().find(|s| {
                                    matches!(s, react_core::session::ThreadStep::User { .. })
                                })
                            {
                                let decision = match last_user {
                                    react_core::session::ThreadStep::User { text, .. } => {
                                        Self::parse_user_decision(text)
                                    }
                                    _ => None,
                                };
                                if decision == Some(UserDecision::Approve) {
                                    if is_cleanse {
                                        let advanced = Self::approve_cleanse_plan_draft_and_advance(
                                            &thread_store,
                                            thread_id,
                                            phase,
                                            &actx,
                                            l.steps.len(),
                                            "plan_approved",
                                            serde_json::json!({ "user_step": last_user }),
                                        )
                                        .await?;
                                        if advanced {
                                            continue;
                                        }
                                        // Idempotent: if we didn't advance (e.g. plan already approved), still proceed.
                                        control_flow::append_phase_with_reason(
                                            &thread_store,
                                            thread_id,
                                            Some("agent".to_string()),
                                            Some(phase),
                                            Phase::CleanseAuthor,
                                            Some("plan_approved"),
                                            Some(serde_json::json!({ "user_step": last_user })),
                                        )
                                        .await?;
                                        continue;
                                    } else {
                                        let advanced = Self::approve_model_plan_draft_and_advance(
                                            &thread_store,
                                            thread_id,
                                            phase,
                                            &actx,
                                            l.steps.len(),
                                            "plan_approved",
                                            serde_json::json!({ "user_step": last_user }),
                                        )
                                        .await?;
                                        if advanced {
                                            continue;
                                        }
                                        control_flow::append_phase_with_reason(
                                            &thread_store,
                                            thread_id,
                                            Some("agent".to_string()),
                                            Some(phase),
                                            Phase::ModelAuthor,
                                            Some("plan_approved"),
                                            Some(serde_json::json!({ "user_step": last_user })),
                                        )
                                        .await?;
                                        continue;
                                    }
                                }
                                if decision == Some(UserDecision::Reject) {
                                    if is_cleanse {
                                        if let Some(mut p) =
                                            crate::data_engineer::plan::load_cleanse_plan(&actx)
                                                .await
                                        {
                                            p.status =
                                                crate::data_engineer::plan::PlanStatus::Cancelled;
                                            let _ = crate::data_engineer::plan::save_cleanse_plan(
                                                &actx, &p,
                                            )
                                            .await;
                                        }
                                    } else {
                                        if let Some(mut p) =
                                            crate::data_engineer::plan::load_model_plan(&actx).await
                                        {
                                            p.status =
                                                crate::data_engineer::plan::PlanStatus::Cancelled;
                                            let _ = crate::data_engineer::plan::save_model_plan(
                                                &actx, &p,
                                            )
                                            .await;
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // If an approved plan already exists (oldest active plan for this thread), move forward (idempotent).
                    if is_cleanse {
                        if let Some(mut p) =
                            crate::data_engineer::plan::load_cleanse_plan(&actx).await
                        {
                            if matches!(
                                p.status,
                                crate::data_engineer::plan::PlanStatus::Approved
                                    | crate::data_engineer::plan::PlanStatus::Completed
                            ) {
                                // Safety: an empty/invalid approved plan would cause authoring to fast-forward.
                                if p.tasks.is_empty() || p.batches.is_empty() {
                                    let plan_key = p.plan_key.clone();
                                    p.status = crate::data_engineer::plan::PlanStatus::Cancelled;
                                    let _ =
                                        crate::data_engineer::plan::save_cleanse_plan(&actx, &p)
                                            .await;
                                    let _ = control_flow::append_phase_with_reason(
                                        &thread_store,
                                        thread_id,
                                        Some("agent".to_string()),
                                        Some(phase),
                                        phase,
                                        Some("plan_invalid_empty"),
                                        Some(serde_json::json!({
                                            "plan_key": plan_key,
                                            "status": format!("{:?}", p.status),
                                            "tasks_len": p.tasks.len(),
                                            "batches_len": p.batches.len(),
                                        })),
                                    )
                                    .await;
                                    continue;
                                }
                                let _ = control_flow::append_phase_with_reason(
                                    &thread_store,
                                    thread_id,
                                    Some("agent".to_string()),
                                    Some(phase),
                                    Phase::CleanseAuthor,
                                    Some("plan_already_approved"),
                                    Some(
                                        serde_json::json!({ "status": format!("{:?}", p.status) }),
                                    ),
                                )
                                .await;
                                continue;
                            }
                        }
                    } else {
                        if let Some(mut p) =
                            crate::data_engineer::plan::load_model_plan(&actx).await
                        {
                            if matches!(
                                p.status,
                                crate::data_engineer::plan::PlanStatus::Approved
                                    | crate::data_engineer::plan::PlanStatus::Completed
                            ) {
                                // Safety: an empty/invalid approved plan would cause authoring to fast-forward.
                                if p.tasks.is_empty() || p.batches.is_empty() {
                                    let plan_key = p.plan_key.clone();
                                    p.status = crate::data_engineer::plan::PlanStatus::Cancelled;
                                    let _ = crate::data_engineer::plan::save_model_plan(&actx, &p)
                                        .await;
                                    let _ = control_flow::append_phase_with_reason(
                                        &thread_store,
                                        thread_id,
                                        Some("agent".to_string()),
                                        Some(phase),
                                        phase,
                                        Some("plan_invalid_empty"),
                                        Some(serde_json::json!({
                                            "plan_key": plan_key,
                                            "status": format!("{:?}", p.status),
                                            "tasks_len": p.tasks.len(),
                                            "batches_len": p.batches.len(),
                                        })),
                                    )
                                    .await;
                                    continue;
                                }
                                let _ = control_flow::append_phase_with_reason(
                                    &thread_store,
                                    thread_id,
                                    Some("agent".to_string()),
                                    Some(phase),
                                    Phase::ModelAuthor,
                                    Some("plan_already_approved"),
                                    Some(
                                        serde_json::json!({ "status": format!("{:?}", p.status) }),
                                    ),
                                )
                                .await;
                                continue;
                            }
                        }
                    }

                    // If there is an existing draft plan (oldest active), re-ask approval rather than creating a new plan.
                    if is_cleanse {
                        if let Some(p) = crate::data_engineer::plan::load_cleanse_plan(&actx).await
                        {
                            if p.status == crate::data_engineer::plan::PlanStatus::Draft {
                                if entered_from_actionable_review {
                                    let mut detail = serde_json::json!({
                                        "plan_key": p.plan_key,
                                        "entry_reason_code": "review_actionable_true"
                                    });
                                    let plan_update = Self::plan_update_summary_cleanse(
                                        prior_cleanse_plan_for_update.as_ref(),
                                        &p,
                                        actionable_review_entry_step_idx,
                                    );
                                    if let Some(idx) = actionable_review_entry_step_idx {
                                        if let Some(obj) = detail.as_object_mut() {
                                            obj.insert(
                                                "entry_step_idx".to_string(),
                                                serde_json::json!(idx),
                                            );
                                        }
                                    }
                                    if let Some(obj) = detail.as_object_mut() {
                                        obj.insert("plan_update_summary".to_string(), plan_update);
                                    }
                                    let advanced = Self::approve_cleanse_plan_draft_and_advance(
                                        &thread_store,
                                        thread_id,
                                        phase,
                                        &actx,
                                        log.as_ref().map(|l| l.steps.len()).unwrap_or(0),
                                        "plan_auto_approved",
                                        detail,
                                    )
                                    .await?;
                                    if advanced {
                                        continue;
                                    }
                                }
                                let prompt = format!(
                                    "{}\n\nApprove this cleanse plan? Reply \"approve\" or \"reject\".\n\n(Plan saved to: {})",
                                    crate::data_engineer::plan::summarize_cleanse_plan(&p, 30),
                                    p.plan_key
                                );
                                return Ok(vec![FlowFrame::AwaitApproval { prompt }]);
                            }
                        }
                    } else {
                        if let Some(p) = crate::data_engineer::plan::load_model_plan(&actx).await {
                            if p.status == crate::data_engineer::plan::PlanStatus::Draft {
                                if entered_from_actionable_review {
                                    let mut detail = serde_json::json!({
                                        "plan_key": p.plan_key,
                                        "entry_reason_code": "review_actionable_true"
                                    });
                                    let plan_update = Self::plan_update_summary_model(
                                        prior_model_plan_for_update.as_ref(),
                                        &p,
                                        actionable_review_entry_step_idx,
                                    );
                                    if let Some(idx) = actionable_review_entry_step_idx {
                                        if let Some(obj) = detail.as_object_mut() {
                                            obj.insert(
                                                "entry_step_idx".to_string(),
                                                serde_json::json!(idx),
                                            );
                                        }
                                    }
                                    if let Some(obj) = detail.as_object_mut() {
                                        obj.insert("plan_update_summary".to_string(), plan_update);
                                    }
                                    let advanced = Self::approve_model_plan_draft_and_advance(
                                        &thread_store,
                                        thread_id,
                                        phase,
                                        &actx,
                                        log.as_ref().map(|l| l.steps.len()).unwrap_or(0),
                                        "plan_auto_approved",
                                        detail,
                                    )
                                    .await?;
                                    if advanced {
                                        continue;
                                    }
                                }
                                let prompt = format!(
                                    "{}\n\nApprove this model plan? Reply \"approve\" or \"reject\".\n\n(Plan saved to: {})",
                                    crate::data_engineer::plan::summarize_model_plan(&p, 30),
                                    p.plan_key
                                );
                                return Ok(vec![FlowFrame::AwaitApproval { prompt }]);
                            }
                        }
                    }

                    // Generate a new draft plan via LLM and then ask the user to approve it.
                    Self::ensure_catalog_bootstrap(sctx).await;
                    // Deterministic bootstrap: ensure the plan phase ALWAYS has grounded context recorded
                    // in the thread history. This prevents LLM loops that repeatedly call dbt_files list
                    // and never reach sql_schema/evidence, which would trip the plan_grounding guard.
                    //
                    // Important: we do NOT decide the plan here; we just provide enough reality to
                    // ground the LLM's plan authoring.
                    let mut bootstrap_summary: Option<String> = None;
                    if let Some(ref l) = log {
                        let start = Self::phase_start_idx(l, phase).unwrap_or(0);
                        let mut saw_dbt_files = false;
                        let mut saw_sql_schema = false;
                        let mut saw_evidence = false;
                        for s in l.steps.iter().skip(start + 1) {
                            let name_opt: Option<&str> = match s {
                                react_core::session::ThreadStep::ToolStart { name, .. } => {
                                    Some(name.as_str())
                                }
                                react_core::session::ThreadStep::ToolEnd { name, .. } => {
                                    Some(name.as_str())
                                }
                                _ => None,
                            };
                            if let Some(name) = name_opt {
                                match name {
                                    "dbt_files" => saw_dbt_files = true,
                                    "sql_schema" => saw_sql_schema = true,
                                    "sql_stats" | "sql_sample" | "run_sql" => saw_evidence = true,
                                    _ => {}
                                }
                            }
                        }
                        if !(saw_dbt_files && saw_sql_schema && saw_evidence) {
                            // Bootstrap calls are intentionally conservative: list models (may be empty),
                            // read core config files, list datasets, and run a minimal probe on one table.
                            let query = sctx
                                .query
                                .as_ref()
                                .ok_or_else(|| "query provider missing".to_string())?
                                .clone();
                            let dbt_files_tool = tools::dbt_files::DbtFilesTool {
                                datasets: sctx.datasets.clone(),
                            };
                            let sql_schema_tool = tools::sql_schema::SqlSchemaTool {
                                query: query.clone(),
                                datasets: sctx.datasets.clone(),
                                catalog: sctx.catalog.clone(),
                            };
                            let run_sql_tool = tools::sql_run::SqlRunTool {
                                query: query.clone(),
                            };

                            let tool_timeout = |name: &str| {
                                actx.policy
                                    .timeout_for_tool(name)
                                    .unwrap_or(actx.per_step_timeout_secs)
                            };
                            let dbt_files_timeout = tool_timeout("dbt_files");
                            let sql_schema_timeout = tool_timeout("sql_schema");
                            let run_sql_timeout = tool_timeout("run_sql");

                            let models_list = control_flow::call_and_record_tool(
                                &thread_store,
                                thread_id,
                                Some("agent".to_string()),
                                &dbt_files_tool,
                                serde_json::json!({"op":"list","prefix":"models/","limit":500}),
                                &actx,
                                dbt_files_timeout,
                            )
                            .await;
                            let _dbt_project = control_flow::call_and_record_tool(
                                &thread_store,
                                thread_id,
                                Some("agent".to_string()),
                                &dbt_files_tool,
                                serde_json::json!({"op":"get","path":"dbt_project.yml","max_chars":4000}),
                                &actx,
                                dbt_files_timeout,
                            )
                            .await;
                            let _packages = control_flow::call_and_record_tool(
                                &thread_store,
                                thread_id,
                                Some("agent".to_string()),
                                &dbt_files_tool,
                                serde_json::json!({"op":"get","path":"packages.yml","max_chars":4000}),
                                &actx,
                                dbt_files_timeout,
                            )
                            .await;
                            let _schema_yml = control_flow::call_and_record_tool(
                                &thread_store,
                                thread_id,
                                Some("agent".to_string()),
                                &dbt_files_tool,
                                serde_json::json!({"op":"get","path":"models/schema.yml","max_chars":6000}),
                                &actx,
                                dbt_files_timeout,
                            )
                            .await;

                            let tables_obs = control_flow::call_and_record_tool(
                                &thread_store,
                                thread_id,
                                Some("agent".to_string()),
                                &sql_schema_tool,
                                serde_json::json!({}),
                                &actx,
                                sql_schema_timeout,
                            )
                            .await;
                            let tables: Vec<String> = tables_obs
                                .get("tables")
                                .and_then(|v| v.as_array())
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                                        .collect()
                                })
                                .unwrap_or_default();

                            let mut probed: Option<String> = None;
                            if let Some(first) = tables.first() {
                                let _cols = control_flow::call_and_record_tool(
                                    &thread_store,
                                    thread_id,
                                    Some("agent".to_string()),
                                    &sql_schema_tool,
                                    serde_json::json!({"table": first}),
                                    &actx,
                                    sql_schema_timeout,
                                )
                                .await;
                                let _cnt = control_flow::call_and_record_tool(
                                    &thread_store,
                                    thread_id,
                                    Some("agent".to_string()),
                                    &run_sql_tool,
                                    serde_json::json!({"sql": format!("SELECT count(*) AS total FROM {}", first)}),
                                    &actx,
                                    run_sql_timeout,
                                )
                                .await;
                                probed = Some(first.clone());
                            }

                            let model_count = models_list
                                .get("items")
                                .and_then(|v| v.as_array())
                                .map(|a| a.len())
                                .unwrap_or(0);
                            let mut head_tables = tables.clone();
                            head_tables.truncate(10);
                            bootstrap_summary = Some(format!(
                                "Deterministic bootstrap (suite-provided):\n- models/ listed: {} item(s)\n- tables discovered (head): {:?}\n- probed table for evidence: {:?}\n\nIf models/ is empty, that's OK; proceed using sql_schema discovery.",
                                model_count,
                                head_tables,
                                probed
                            ));
                        }
                    }
                    let sys = crate::util::time_context::with_time_context(if is_cleanse {
                        prompts::cleanse_plan_system_prompt()
                    } else {
                        prompts::model_plan_system_prompt()
                    });
                    let (registry, tools_card) =
                        Self::build_tools_for_phase(phase, &guard, allow_ask_approval, sctx, None)?;

                    let mut q = if is_cleanse {
                        format!(
                            "Create a SILVER/cleanse execution plan (batched in groups of 5).\n\nOriginal goal:\n{}\n",
                            question
                        )
                    } else {
                        format!(
                            "Create a GOLD/model execution plan (batched in groups of 5) based ONLY on existing silver/staging models.\n\nOriginal goal:\n{}\n",
                            question
                        )
                    };

                    // Inject global preflight semantic context/audiences (if available).
                    // CRITICAL: global_semantic_context is intended for GOLD model planning only,
                    // not for SILVER/cleanse planning.
                    if !is_cleanse {
                        let global_key = sctx.keyspace.semantic_key(
                            &sctx.scope,
                            react_core::providers::catalog::types::GLOBAL_SEMANTIC_DATASET_ID,
                        );
                        if let Ok(v) = sctx.storage.get_json(&global_key).await {
                            q.push_str("\n\nIMMUTABLE CONTEXT (global_semantic_context):\n");
                            q.push_str(
                                &serde_json::to_string_pretty(&v)
                                    .unwrap_or_else(|_| "{}".to_string()),
                            );
                            q.push('\n');
                        }
                    }
                    // If this plan phase was entered because review produced actionable feedback,
                    // include that feedback verbatim to ground the new plan.
                    if let Some(ref l) = log {
                        if let Some(step) = l.steps.iter().rev().find(|s| match s {
                            react_core::session::ThreadStep::Phase { phase: p, .. } => {
                                p == phase.as_str()
                            }
                            _ => false,
                        }) {
                            let (reason_code, reason_detail) = match step {
                                react_core::session::ThreadStep::Phase {
                                    reason_code,
                                    reason_detail,
                                    ..
                                } => (reason_code.as_deref().unwrap_or(""), reason_detail.as_ref()),
                                _ => ("", None),
                            };
                            if reason_code == "review_actionable_true" {
                                if let Some(idx) = actionable_review_entry_step_idx {
                                    q.push_str(&format!(
                                        "\nReview entry step idx: {}\nIMPORTANT: Any NEW checklist items introduced due to this review MUST set origin=\"review_actionable\" and origin_step_idx={}.\n",
                                        idx, idx
                                    ));
                                }
                                if let Some(key) = reason_detail
                                    .and_then(|v| v.get("review_ref"))
                                    .and_then(|v| v.get("key"))
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.trim().to_string())
                                    .filter(|s| !s.is_empty())
                                {
                                    if let Ok(bytes) = actx.storage.get_bytes(&key).await {
                                        let txt = String::from_utf8_lossy(&bytes).to_string();
                                        if !txt.trim().is_empty() {
                                            q.push_str("\nReview feedback to incorporate in this plan:\n");
                                            q.push_str(txt.trim());
                                            q.push('\n');
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if let Some(ref brief) = last_validate_brief {
                        q.push_str("\nLast dbt_validate error summary (if any):\n");
                        q.push_str(brief);
                        q.push('\n');
                    }
                    if let Some(bs) = bootstrap_summary.as_ref() {
                        q.push_str("\n\n");
                        q.push_str(bs);
                        q.push('\n');
                    }
                    // Plan mode should be especially broad: include the full available relation list (bounded)
                    // so planning never needs to guess table names.
                    if let Some(ds) = sctx.datasets.as_ref() {
                        if let Ok(items) = ds.list_datasets().await {
                            let mut tables: Vec<String> =
                                items.into_iter().map(|d| d.fqn()).collect();
                            tables.sort();
                            if !tables.is_empty() {
                                q.push_str("\n\nIMMUTABLE FACTS (available_relations, bounded):\n");
                                q.push_str(
                                    &serde_json::to_string_pretty(&serde_json::json!({
                                        "tables": tables.into_iter().take(300).collect::<Vec<_>>()
                                    }))
                                    .unwrap_or_else(|_| "{}".to_string()),
                                );
                                q.push('\n');
                            }
                        }
                    }

                    let llm_options = if is_cleanse {
                        Some(LlmCallOptions {
                            temperature: Some(0.20),
                            top_p: Some(1.0),
                            max_output_tokens: Some(2200),
                        })
                    } else {
                        Some(LlmCallOptions {
                            temperature: Some(0.55),
                            top_p: Some(0.95),
                            max_output_tokens: Some(2800),
                        })
                    };
                    match Agent::run_until_block(&registry, &actx, &sys, &tools_card, &q, llm_options).await {
                        Ok(RunOutcome::Final {
                            thread_id: _tid,
                            result,
                        }) => {
                            // Deterministic enforcement: planning must be grounded in actual project state.
                            // Require at least:
                            // - 1 dbt_files call
                            // - 1 sql_schema call
                            // - 1 of (sql_stats/sql_sample/run_sql)
                            if let Ok(ref l) = thread_store.get(thread_id).await {
                                let start = Self::phase_start_idx(l, phase).unwrap_or(0);
                                let mut saw_dbt_files = false;
                                let mut saw_sql_schema = false;
                                let mut saw_evidence = false;
                                for s in l.steps.iter().skip(start + 1) {
                                    let name_opt: Option<&str> = match s {
                                        react_core::session::ThreadStep::ToolStart {
                                            name, ..
                                        } => Some(name.as_str()),
                                        react_core::session::ThreadStep::ToolEnd {
                                            name, ..
                                        } => Some(name.as_str()),
                                        _ => None,
                                    };
                                    if let Some(name) = name_opt {
                                        match name {
                                            "dbt_files" => saw_dbt_files = true,
                                            "sql_schema" => saw_sql_schema = true,
                                            "sql_stats" | "sql_sample" | "run_sql" => {
                                                saw_evidence = true
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                                if !(saw_dbt_files && saw_sql_schema && saw_evidence) {
                                    // Stay in plan phase and re-run with a hard reminder.
                                    let miss = format!(
                                        "Plan is missing required grounding steps.\n\
                                         Required before finalizing a plan:\n\
                                         - dbt_files (inventory existing dbt project)\n\
                                         - sql_schema (list tables)\n\
                                         - evidence via sql_stats/sql_sample/run_sql\n\n\
                                         Seen: dbt_files={saw_dbt_files}, sql_schema={saw_sql_schema}, evidence={saw_evidence}\n\
                                         Please retry plan generation and include those discovery steps."
                                    );
                                    let ts = chrono::Utc::now().to_rfc3339();
                                    let step = react_core::session::ThreadStep::GuardBlock {
                                        phase: phase.as_str().to_string(),
                                        kind: "plan_grounding".to_string(),
                                        reason: miss.clone(),
                                        observation: react_core::session::Observation::fail(vec![
                                            miss.clone(),
                                        ]),
                                        ts,
                                        agent: "agent".to_string(),
                                    };
                                    let _ = thread_store.append_step(thread_id, step.clone()).await;
                                    control_flow::append_phase_with_reason(
                                        &thread_store,
                                        thread_id,
                                        Some("agent".to_string()),
                                        Some(phase),
                                        phase,
                                        Some("phase_blocked"),
                                        Some(serde_json::json!({
                                            "kind": "plan_grounding",
                                            "reason": miss,
                                            "trigger_step": step,
                                        })),
                                    )
                                    .await?;
                                    continue;
                                }
                            }

                            if is_cleanse {
                                if result.kind != "cleanse_plan" {
                                    return Ok(vec![FlowFrame::AwaitUser {
                                        prompt: format!(
                                            "Plan phase failed: final.kind must be 'cleanse_plan' (got '{}'). Please retry.",
                                            result.kind
                                        ),
                                    }]);
                                }
                                let mut payload = result.payload.clone();
                                let mut plan_opt: Option<crate::data_engineer::plan::CleansePlan> =
                                    None;
                                let mut last_err: Option<String> = None;
                                for attempt in 1..=3 {
                                    match serde_json::from_value::<
                                        crate::data_engineer::plan::CleansePlan,
                                    >(payload.clone())
                                    {
                                        Ok(p) => {
                                            plan_opt = Some(p);
                                            last_err = None;
                                            break;
                                        }
                                        Err(e) => {
                                            let err = format!("invalid cleanse plan JSON: {e}");
                                            last_err = Some(err.clone());
                                            // Ask the LLM to repair the plan JSON and re-emit it.
                                            let repaired = Self::repair_plan_json_payload_via_llm(
                                                &thread_store,
                                                thread_id,
                                                &actx,
                                                phase,
                                                "cleanse_plan",
                                                &payload,
                                                &err,
                                                attempt,
                                            )
                                            .await?;
                                            payload = repaired.payload;
                                            // Loop again: we will re-try serde parsing on the repaired payload.
                                        }
                                    }
                                }
                                if let Some(e) = last_err {
                                    return Ok(vec![FlowFrame::AwaitUser {
                                        prompt: format!(
                                            "Plan JSON is invalid and could not be repaired automatically.\n\nError:\n{e}\n\nPlease reply with a corrected STRICT JSON final envelope for kind='cleanse_plan'."
                                        ),
                                    }]);
                                }
                                let mut plan = plan_opt.ok_or_else(|| {
                                    "plan json repair: no plan produced".to_string()
                                })?;
                                // Planning/repair must not emit evidence; the deterministic runner adds it later.
                                for t in plan.tasks.iter_mut() {
                                    for it in t.checklist.iter_mut() {
                                        it.evidence.clear();
                                    }
                                }
                                plan.status = crate::data_engineer::plan::PlanStatus::Draft;
                                plan.plan_key =
                                    crate::data_engineer::plan::new_cleanse_plan_key(&actx);
                                // Scope progress to the current plan instance so we don't replay the full
                                // historical log and accidentally mark tasks done from prior cycles.
                                plan.progress.last_applied_step_idx =
                                    log.as_ref().map(|l| l.steps.len()).unwrap_or(0);
                                // Capture a cheap snapshot for “current project as-is” provenance.
                                plan.project_snapshot = serde_json::json!({
                                    "dbt_prefix": actx.keyspace.dbt_prefix(&actx.scope),
                                    "dbt_project_yml_etag": actx.storage.head_etag(&actx.keyspace.dbt_project_key(&actx.scope)).await.ok().flatten(),
                                });
                                if entered_from_actionable_review {
                                    if let Some(obj) = plan.project_snapshot.as_object_mut() {
                                        obj.insert(
                                            "entry_reason_code".to_string(),
                                            serde_json::json!("review_actionable_true"),
                                        );
                                        if let Some(idx) = actionable_review_entry_step_idx {
                                            obj.insert(
                                                "entry_step_idx".to_string(),
                                                serde_json::json!(idx),
                                            );
                                        }
                                    }
                                }
                                // Persist schema facts observed during this plan phase (no ambiguity).
                                if let Some(ref l) = log {
                                    let start = Self::phase_start_idx(l, phase).unwrap_or(0);
                                    let mut calls: Vec<serde_json::Value> = Vec::new();
                                    for s in l.steps.iter().skip(start + 1) {
                                        let react_core::session::ThreadStep::ToolEnd {
                                            name,
                                            args,
                                            observation,
                                            ..
                                        } = s
                                        else {
                                            continue;
                                        };
                                        if name != "sql_schema" || !observation.ok {
                                            continue;
                                        }
                                        let table = args
                                            .get("table")
                                            .and_then(|v| v.as_str())
                                            .map(|s| s.trim().to_string());
                                        let mut rec = serde_json::json!({
                                            "tool": "sql_schema",
                                            "table": table,
                                            "result": observation.extra,
                                        });
                                        // Keep deterministic ordering by dropping null table field when absent.
                                        if rec
                                            .get("table")
                                            .and_then(|v| v.as_str())
                                            .map(|s| s.is_empty())
                                            .unwrap_or(false)
                                        {
                                            if let Some(obj) = rec.as_object_mut() {
                                                obj.remove("table");
                                            }
                                        }
                                        calls.push(rec);
                                        if calls.len() >= 120 {
                                            break;
                                        }
                                    }
                                    if !calls.is_empty() {
                                        if plan.project_snapshot.is_null() {
                                            plan.project_snapshot = serde_json::json!({});
                                        }
                                        if let Some(obj) = plan.project_snapshot.as_object_mut() {
                                            obj.insert(
                                                "plan_schema_facts".to_string(),
                                                serde_json::json!({ "sql_schema_calls": calls }),
                                            );
                                        }
                                    }
                                }
                                // Ground the plan against reality: only keep datasets we can prove exist via schema().
                                let mut candidates: Vec<String> = Vec::new();
                                for t in plan.tasks.iter() {
                                    if !t.dataset_id.trim().is_empty() {
                                        candidates.push(t.dataset_id.trim().to_string());
                                    }
                                }
                                for b in plan.batches.iter() {
                                    for ds in b.iter() {
                                        if !ds.trim().is_empty() {
                                            candidates.push(ds.trim().to_string());
                                        }
                                    }
                                }
                                candidates.sort();
                                candidates.dedup();
                                let grounded =
                                    crate::data_engineer::dataset_truth::build_grounded_raw_dataset_set(&actx, &actx.warehouse, &candidates)
                                        .await;
                                crate::data_engineer::plan::prune_cleanse_plan_to_grounded_raw_datasets(
                                    &mut plan,
                                    &grounded.allowed,
                                );
                                if plan.tasks.is_empty() || plan.batches.is_empty() {
                                    return Ok(vec![FlowFrame::AwaitUser {
                                        prompt: "Cleanse plan contained no grounded raw datasets after applying schema() facts. This indicates the raw datasets are not queryable via the current warehouse connection (or the plan referenced non-raw tables). Fix the underlying data/catalog visibility and retry plan generation."
                                            .to_string(),
                                    }]);
                                }
                                crate::data_engineer::plan::save_cleanse_plan(&actx, &plan).await?;
                                if entered_from_actionable_review {
                                    let mut detail = serde_json::json!({
                                        "plan_key": plan.plan_key,
                                        "entry_reason_code": "review_actionable_true"
                                    });
                                    let plan_update = Self::plan_update_summary_cleanse(
                                        prior_cleanse_plan_for_update.as_ref(),
                                        &plan,
                                        actionable_review_entry_step_idx,
                                    );
                                    if let Some(idx) = actionable_review_entry_step_idx {
                                        if let Some(obj) = detail.as_object_mut() {
                                            obj.insert(
                                                "entry_step_idx".to_string(),
                                                serde_json::json!(idx),
                                            );
                                        }
                                    }
                                    if let Some(obj) = detail.as_object_mut() {
                                        obj.insert("plan_update_summary".to_string(), plan_update);
                                    }
                                    let advanced = Self::approve_cleanse_plan_draft_and_advance(
                                        &thread_store,
                                        thread_id,
                                        phase,
                                        &actx,
                                        log.as_ref().map(|l| l.steps.len()).unwrap_or(0),
                                        "plan_auto_approved",
                                        detail,
                                    )
                                    .await?;
                                    if advanced {
                                        continue;
                                    }
                                }
                                let prompt = format!(
                                    "{}\n\nApprove this cleanse plan? Reply \"approve\" or \"reject\".\n\n(Plan saved to: {})",
                                    crate::data_engineer::plan::summarize_cleanse_plan(&plan, 30),
                                    plan.plan_key
                                );
                                return Ok(vec![FlowFrame::AwaitApproval { prompt }]);
                            } else {
                                if result.kind != "model_plan" {
                                    return Ok(vec![FlowFrame::AwaitUser {
                                        prompt: format!(
                                            "Plan phase failed: final.kind must be 'model_plan' (got '{}'). Please retry.",
                                            result.kind
                                        ),
                                    }]);
                                }
                                let mut payload = result.payload.clone();
                                let mut plan_opt: Option<crate::data_engineer::plan::ModelPlan> =
                                    None;
                                let mut last_err: Option<String> = None;
                                for attempt in 1..=3 {
                                    match serde_json::from_value::<
                                        crate::data_engineer::plan::ModelPlan,
                                    >(payload.clone())
                                    {
                                        Ok(p) => {
                                            plan_opt = Some(p);
                                            last_err = None;
                                            break;
                                        }
                                        Err(e) => {
                                            let err = format!("invalid model plan JSON: {e}");
                                            last_err = Some(err.clone());
                                            // Ask the LLM to repair the plan JSON and re-emit it.
                                            let repaired = Self::repair_plan_json_payload_via_llm(
                                                &thread_store,
                                                thread_id,
                                                &actx,
                                                phase,
                                                "model_plan",
                                                &payload,
                                                &err,
                                                attempt,
                                            )
                                            .await?;
                                            payload = repaired.payload;
                                            // Loop again: we will re-try serde parsing on the repaired payload.
                                        }
                                    }
                                }
                                if let Some(e) = last_err {
                                    return Ok(vec![FlowFrame::AwaitUser {
                                        prompt: format!(
                                            "Plan JSON is invalid and could not be repaired automatically.\n\nError:\n{e}\n\nPlease reply with a corrected STRICT JSON final envelope for kind='model_plan'."
                                        ),
                                    }]);
                                }
                                let mut plan = plan_opt.ok_or_else(|| {
                                    "plan json repair: no plan produced".to_string()
                                })?;
                                // Planning/repair must not emit evidence; the deterministic runner adds it later.
                                for t in plan.tasks.iter_mut() {
                                    for it in t.checklist.iter_mut() {
                                        it.evidence.clear();
                                    }
                                }
                                plan.status = crate::data_engineer::plan::PlanStatus::Draft;
                                plan.plan_key =
                                    crate::data_engineer::plan::new_model_plan_key(&actx);
                                // Scope progress to the current plan instance so we don't replay the full
                                // historical log and accidentally mark tasks done from prior cycles.
                                plan.progress.last_applied_step_idx =
                                    log.as_ref().map(|l| l.steps.len()).unwrap_or(0);
                                plan.project_snapshot = serde_json::json!({
                                    "dbt_prefix": actx.keyspace.dbt_prefix(&actx.scope),
                                    "dbt_project_yml_etag": actx.storage.head_etag(&actx.keyspace.dbt_project_key(&actx.scope)).await.ok().flatten(),
                                });
                                if entered_from_actionable_review {
                                    if let Some(obj) = plan.project_snapshot.as_object_mut() {
                                        obj.insert(
                                            "entry_reason_code".to_string(),
                                            serde_json::json!("review_actionable_true"),
                                        );
                                        if let Some(idx) = actionable_review_entry_step_idx {
                                            obj.insert(
                                                "entry_step_idx".to_string(),
                                                serde_json::json!(idx),
                                            );
                                        }
                                    }
                                }
                                // Persist schema facts observed during this plan phase (no ambiguity).
                                if let Some(ref l) = log {
                                    let start = Self::phase_start_idx(l, phase).unwrap_or(0);
                                    let mut calls: Vec<serde_json::Value> = Vec::new();
                                    for s in l.steps.iter().skip(start + 1) {
                                        let react_core::session::ThreadStep::ToolEnd {
                                            name,
                                            args,
                                            observation,
                                            ..
                                        } = s
                                        else {
                                            continue;
                                        };
                                        if name != "sql_schema" || !observation.ok {
                                            continue;
                                        }
                                        let table = args
                                            .get("table")
                                            .and_then(|v| v.as_str())
                                            .map(|s| s.trim().to_string());
                                        let mut rec = serde_json::json!({
                                            "tool": "sql_schema",
                                            "table": table,
                                            "result": observation.extra,
                                        });
                                        if rec
                                            .get("table")
                                            .and_then(|v| v.as_str())
                                            .map(|s| s.is_empty())
                                            .unwrap_or(false)
                                        {
                                            if let Some(obj) = rec.as_object_mut() {
                                                obj.remove("table");
                                            }
                                        }
                                        calls.push(rec);
                                        if calls.len() >= 120 {
                                            break;
                                        }
                                    }
                                    if !calls.is_empty() {
                                        if plan.project_snapshot.is_null() {
                                            plan.project_snapshot = serde_json::json!({});
                                        }
                                        if let Some(obj) = plan.project_snapshot.as_object_mut() {
                                            obj.insert(
                                                "plan_schema_facts".to_string(),
                                                serde_json::json!({ "sql_schema_calls": calls }),
                                            );
                                        }
                                    }
                                }
                                // Ground gold planning: gold must be based ONLY on existing staging (silver) models.
                                let stg = crate::data_engineer::dataset_truth::discover_staging_models_from_storage(&actx).await;
                                crate::data_engineer::plan::prune_model_plan_to_grounded_staging_models(
                                    &mut plan,
                                    &stg.allowed_models,
                                );
                                if plan.tasks.is_empty() || plan.batches.is_empty() {
                                    return Ok(vec![FlowFrame::AwaitUser {
                                        prompt: "Model plan contained no grounded tasks after enforcing gold-tier constraints (must reference existing staging/silver models only). Ensure staging models exist under models/staging/ and retry."
                                            .to_string(),
                                    }]);
                                }
                                crate::data_engineer::plan::save_model_plan(&actx, &plan).await?;
                                if entered_from_actionable_review {
                                    let mut detail = serde_json::json!({
                                        "plan_key": plan.plan_key,
                                        "entry_reason_code": "review_actionable_true"
                                    });
                                    let plan_update = Self::plan_update_summary_model(
                                        prior_model_plan_for_update.as_ref(),
                                        &plan,
                                        actionable_review_entry_step_idx,
                                    );
                                    if let Some(idx) = actionable_review_entry_step_idx {
                                        if let Some(obj) = detail.as_object_mut() {
                                            obj.insert(
                                                "entry_step_idx".to_string(),
                                                serde_json::json!(idx),
                                            );
                                        }
                                    }
                                    if let Some(obj) = detail.as_object_mut() {
                                        obj.insert("plan_update_summary".to_string(), plan_update);
                                    }
                                    let advanced = Self::approve_model_plan_draft_and_advance(
                                        &thread_store,
                                        thread_id,
                                        phase,
                                        &actx,
                                        log.as_ref().map(|l| l.steps.len()).unwrap_or(0),
                                        "plan_auto_approved",
                                        detail,
                                    )
                                    .await?;
                                    if advanced {
                                        continue;
                                    }
                                }
                                let prompt = format!(
                                    "{}\n\nApprove this model plan? Reply \"approve\" or \"reject\".\n\n(Plan saved to: {})",
                                    crate::data_engineer::plan::summarize_model_plan(&plan, 30),
                                    plan.plan_key
                                );
                                return Ok(vec![FlowFrame::AwaitApproval { prompt }]);
                            }
                        }
                        Ok(RunOutcome::AwaitUser {
                            thread_id: _tid,
                            prompt,
                        }) => {
                            return Ok(vec![FlowFrame::AwaitUser { prompt }]);
                        }
                        Ok(RunOutcome::AwaitApproval {
                            thread_id: _tid,
                            prompt,
                        }) => {
                            // Planning should not directly request approval via tools; the suite does it.
                            return Ok(vec![FlowFrame::AwaitUser {
                                prompt: format!(
                                    "Plan phase requested approval internally, which is not allowed. Please retry.\n\nPrompt:\n{}",
                                    prompt
                                ),
                            }]);
                        }
                        Err(e) => return Err(e),
                    }
                }

                Phase::CleanseAuthor | Phase::ModelAuthor => {
                    let is_cleanse = phase == Phase::CleanseAuthor;
                    // Treat schema precheck failures as "validate failed" for authoring guard behavior.
                    // Otherwise we can bounce Author->Validate->Author without requiring a mutation.
                    let entered_from_precheck_failed = log.as_ref().and_then(|l| {
                        l.steps.iter().rev().find_map(|s| match s {
                            react_core::session::ThreadStep::Phase {
                                phase: p,
                                reason_code,
                                ..
                            } if p == phase.as_str() => reason_code.as_deref(),
                            _ => None,
                        })
                    }) == Some("precheck_failed");
                    let mut phase_guard = guard.clone();
                    if entered_from_precheck_failed {
                        phase_guard.last_validate_failed = true;
                        phase_guard.mutated_since_fail = false;
                    }
                    let hard_mutation_repair_mode =
                        phase_guard.last_validate_failed && !phase_guard.mutated_since_fail;
                    let last_guard_reason = log.as_ref().and_then(|l| {
                        l.steps.iter().rev().find_map(|s| match s {
                            react_core::session::ThreadStep::GuardBlock { reason, .. } => {
                                Some(reason.as_str())
                            }
                            _ => None,
                        })
                    });
                    let failure_class = classify_validate_failure(
                        entered_from_precheck_failed,
                        last_validate_brief.as_deref(),
                        last_guard_reason,
                    );
                    let prefer_schema_repairs =
                        matches!(failure_class, ValidateFailureClass::SchemaOrPrecheck);
                    let sys = crate::util::time_context::with_time_context(if is_cleanse {
                        prompts::cleanse_system_prompt()
                    } else {
                        prompts::model_system_prompt()
                    });
                    let mut actx = AgentCtx {
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
                        warehouse: sctx.warehouse.clone(),
                        dbt: sctx.dbt.clone(),
                        vector: sctx.vector.clone(),
                        thread_store: Some(thread_store.clone()),
                        exec_ctx: None,
                        runtime: sctx
                            .resolved_config
                            .clone()
                            .map(|c| c as Arc<dyn std::any::Any + Send + Sync>),
                    };

                    // Plan-driven batching: load the approved plan, update progress from the thread log,
                    // and compute the exact next batch to execute (max 5).
                    let (plan_context, allowed_batch): (String, Option<AllowedBatch>) =
                        if is_cleanse {
                            let mut plan = match crate::data_engineer::plan::load_cleanse_plan_any(
                                &actx,
                            )
                            .await
                            {
                                Some(p) => p,
                                None => {
                                    // Hard fail: authoring was entered, so we EXPECT an approved plan to exist.
                                    // Missing plan indicates storage drift or a corrupted thread state and should not loop.
                                    return Err("missing active cleanse plan in authoring phase (expected plan to exist)".to_string());
                                }
                            };
                            // Safety: do not allow authoring to proceed with an empty/invalid approved plan,
                            // otherwise the phase will fast-forward (plan_tasks_done) without doing work.
                            if plan.tasks.is_empty() || plan.batches.is_empty() {
                                let plan_key = plan.plan_key.clone();
                                plan.status = crate::data_engineer::plan::PlanStatus::Cancelled;
                                let _ = crate::data_engineer::plan::save_cleanse_plan(&actx, &plan)
                                    .await;
                                control_flow::append_phase_with_reason(
                                    &thread_store,
                                    thread_id,
                                    Some("agent".to_string()),
                                    Some(phase),
                                    Phase::CleansePlan,
                                    Some("plan_invalid_empty"),
                                    Some(serde_json::json!({
                                        "plan_key": plan_key,
                                        "tasks_len": plan.tasks.len(),
                                        "batches_len": plan.batches.len(),
                                    })),
                                )
                                .await;
                                continue;
                            }
                            if let Some(ref l) = log {
                                crate::data_engineer::plan::update_cleanse_progress_from_log(
                                    &mut plan, l,
                                );
                                let _ = crate::data_engineer::plan::save_cleanse_plan(&actx, &plan)
                                    .await;
                            }

                            // Explicit execution context for hierarchical UI (best-effort).
                            let next_item =
                                crate::data_engineer::plan::cleanse_next_work_item_ctx(&plan);
                            actx.exec_ctx = Some(react_core::session::ExecutionContext {
                                plan_kind: Some("cleanse".to_string()),
                                plan_key: Some(plan.plan_key.clone()),
                                workgroup_id: next_item.as_ref().map(|x| x.workgroup_id.clone()),
                                task_id: next_item.as_ref().map(|x| x.task_id.clone()),
                                checklist_item_id: next_item
                                    .as_ref()
                                    .map(|x| x.checklist_item_id.clone()),
                            });
                            // Hard stop: if plan-batched authoring is locked due to too many consecutive failures,
                            // return control to the user with a single actionable message (do not loop).
                            if plan.progress.consecutive_batch_failures
                            >= crate::data_engineer::tools::apply_next_batch::MAX_CONSECUTIVE_BATCH_FAILURES
                        {
                            let next = match crate::data_engineer::plan::cleanse_next_action(&plan)
                            {
                                Some((crate::data_engineer::plan::WorkGroupKind::AuthorSql, ds)) => {
                                    ds
                                }
                                _ => crate::data_engineer::plan::cleanse_next_batch(&plan),
                            };
                            let mut expected_paths: Vec<String> = Vec::new();
                            for ds in next.iter() {
                                if let Some(t) = plan.tasks.iter().find(|t| t.dataset_id == *ds) {
                                    if let Some(p) = t.expected_model_path.as_deref() {
                                        if !p.trim().is_empty() {
                                            expected_paths.push(p.trim().to_string());
                                        }
                                    }
                                }
                            }
                            expected_paths.sort();
                            expected_paths.dedup();
                            let reason = lock_prompt_for_plan(
                                "cleanse",
                                &plan.plan_key,
                                plan.progress.consecutive_batch_failures,
                                plan.progress.total_batch_failures,
                                &next,
                                &expected_paths,
                            );
                            let ts = chrono::Utc::now().to_rfc3339();
                            let step = react_core::session::ThreadStep::GuardBlock {
                                phase: phase.as_str().to_string(),
                                kind: "batch_locked".to_string(),
                                reason: reason.clone(),
                                observation: react_core::session::Observation::fail(vec![reason.clone()]),
                                ts,
                                agent: "agent".to_string(),
                            };
                            let _ = thread_store.append_step(thread_id, step).await;
                            return Ok(vec![FlowFrame::AwaitUser { prompt: reason }]);
                        }
                            if plan.status != crate::data_engineer::plan::PlanStatus::Approved
                                && plan.status != crate::data_engineer::plan::PlanStatus::Completed
                            {
                                control_flow::append_phase_with_reason(
                                &thread_store,
                                thread_id,
                                Some("agent".to_string()),
                                Some(phase),
                                Phase::CleansePlan,
                                Some("plan_not_approved"),
                                Some(serde_json::json!({ "status": format!("{:?}", plan.status) })),
                            )
                            .await;
                                continue;
                            }
                            // Work-group driven selection (preferred). Fall back to batch scanning if work_groups
                            // are absent (older plans).
                            let next_action =
                                crate::data_engineer::plan::cleanse_next_action(&plan);
                            let next = match next_action.as_ref() {
                                Some((crate::data_engineer::plan::WorkGroupKind::AuthorSql, ds)) => {
                                    ds.clone()
                                }
                                _ => crate::data_engineer::plan::cleanse_next_batch(&plan),
                            };
                            // IMPORTANT: If validation failed and we have not successfully mutated since,
                            // the authoring tool registry will be patch-only (hard_mutation_only).
                            // In that state, do NOT instruct apply_next_cleanse_batch; force repair-mode guidance.
                            if hard_mutation_repair_mode && !prefer_schema_repairs {
                                // Repair-first routing: dbt_validate failed for a SQL/runtime-class reason.
                                // Even if schema checklist work remains, fix failing SQL targets first.
                                let mut ctx = format!(
                                    "Approved cleanse plan (stored at: {}).\nThe last dbt_validate failed and a mutating fix is required before any further validation.\n\nNext action: call dbt_files op=patch (replace_file/replace_range/replace_list) to fix the failing SQL target(s) below. Keep changes minimal.\n\nRepair targets:\n",
                                    plan.plan_key
                                );
                                if !last_validate_failed_models.is_empty() {
                                    for fm in last_validate_failed_models.iter().take(6) {
                                        let name = fm
                                            .get("name")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("unknown_model");
                                        let file = fm
                                            .get("file")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("(unknown file)");
                                        ctx.push_str(&format!("- {} ({})\n", name, file));
                                    }
                                } else if let Some(ref brief) = last_validate_brief {
                                    ctx.push_str("- (unknown failing model) — see last dbt_validate summary below.\n");
                                    ctx.push_str("\nLast dbt_validate summary:\n");
                                    ctx.push_str(brief);
                                    ctx.push('\n');
                                } else {
                                    ctx.push_str("- (unknown failing model) — no failing-model evidence found.\n");
                                }
                                ctx.push_str("\nIMPORTANT: Defer any new checklist expansion or schema contract work until dbt_validate passes.\n");
                                (ctx, None)
                            } else if hard_mutation_repair_mode {
                                // Schema/precheck failures: prefer schema batch tools when schema checklist work remains.
                                if let Some((
                                    crate::data_engineer::plan::WorkGroupKind::AuthorSchema,
                                    ids,
                                )) = next_action.as_ref()
                                {
                                    let checklist_item_id = actx
                                        .exec_ctx
                                        .as_ref()
                                        .and_then(|c| c.checklist_item_id.as_deref())
                                        .unwrap_or(crate::data_engineer::plan::CHECKLIST_SCHEMA_CONTRACT)
                                        .trim()
                                        .to_string();
                                    let mut expected_paths: Vec<String> = Vec::new();
                                    for ds in ids.iter() {
                                        if let Some(t) = plan.tasks.iter().find(|t| t.dataset_id == *ds) {
                                            if let Some(p) = t.expected_model_path.as_deref() {
                                                if !p.trim().is_empty() {
                                                    expected_paths.push(p.trim().to_string());
                                                }
                                            }
                                        }
                                    }
                                    expected_paths.sort();
                                    expected_paths.dedup();
                                    let mut ctx = format!(
                                        "Approved cleanse plan (stored at: {}).\nPending schema checklist work (checklist_item_id={} ; max 5):\n- {}\n\nNext action: call apply_next_cleanse_schema_batch (do NOT call dbt_files directly).\n\nExpected model SQL paths:\n- {}\n",
                                        plan.plan_key,
                                        checklist_item_id,
                                        ids.join("\n- "),
                                        expected_paths.join("\n- "),
                                    );
                                    ctx.push_str("\nIMPORTANT: Do NOT call apply_next_cleanse_batch while schema checklist work remains; that tool only authors SQL.\n");
                                    (ctx, None)
                                } else {
                                    let pending_schema =
                                        crate::data_engineer::plan::cleanse_pending_for_checklist(
                                            &plan,
                                            crate::data_engineer::plan::CHECKLIST_SQL_MODEL,
                                            crate::data_engineer::plan::CHECKLIST_SCHEMA_CONTRACT,
                                        );
                                    if !pending_schema.is_empty() {
                                        let checklist_item_id = crate::data_engineer::plan::CHECKLIST_SCHEMA_CONTRACT;
                                        let mut expected_paths: Vec<String> = Vec::new();
                                        for ds in pending_schema.iter() {
                                            if let Some(t) = plan.tasks.iter().find(|t| t.dataset_id == *ds) {
                                                if let Some(p) = t.expected_model_path.as_deref() {
                                                    if !p.trim().is_empty() {
                                                        expected_paths.push(p.trim().to_string());
                                                    }
                                                }
                                            }
                                        }
                                        expected_paths.sort();
                                        expected_paths.dedup();
                                        let mut ctx = format!(
                                            "Approved cleanse plan (stored at: {}).\nPending schema checklist work (checklist_item_id={} ; max 5):\n- {}\n\nNext action: call apply_next_cleanse_schema_batch (do NOT call dbt_files directly).\n\nExpected model SQL paths:\n- {}\n",
                                            plan.plan_key,
                                            checklist_item_id,
                                            pending_schema.join("\n- "),
                                            expected_paths.join("\n- "),
                                        );
                                        ctx.push_str("\nIMPORTANT: Do NOT call apply_next_cleanse_batch while schema checklist work remains; that tool only authors SQL.\n");
                                        (ctx, None)
                                    } else {
                                let mut ctx = format!(
                                "Approved cleanse plan (stored at: {}).\nThe last dbt_validate failed and a mutating fix is required before any further validation.\n\nRepair targets (fix these DBT files directly with dbt_files op=patch using replace_file/replace_range/replace_list):\n",
                                plan.plan_key
                            );
                                if !last_validate_failed_models.is_empty() {
                                    for fm in last_validate_failed_models.iter().take(6) {
                                        let name = fm
                                            .get("name")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("unknown_model");
                                        let file = fm
                                            .get("file")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("(unknown file)");
                                        ctx.push_str(&format!("- {} ({})\n", name, file));
                                    }
                                } else if let Some(ref brief) = last_validate_brief {
                                    ctx.push_str("- (unknown failing model) — see last dbt_validate summary below.\n");
                                    ctx.push_str("\nLast dbt_validate summary:\n");
                                    ctx.push_str(brief);
                                    ctx.push('\n');
                                } else {
                                    ctx.push_str("- (unknown failing model) — no failing-model evidence found.\n");
                                }
                                (ctx, None)
                                    }
                                }
                            } else if next.is_empty() {
                                // If work-groups exist, interpret "no next SQL batch" as:
                                // - either we're blocked on schema checklist authoring, OR
                                // - we're ready to transition to validate.
                                if let Some((
                                    crate::data_engineer::plan::WorkGroupKind::AuthorSchema,
                                    ids,
                                )) = next_action.as_ref()
                                {
                                    let checklist_item_id = actx
                                        .exec_ctx
                                        .as_ref()
                                        .and_then(|c| c.checklist_item_id.as_deref())
                                        .unwrap_or(crate::data_engineer::plan::CHECKLIST_SCHEMA_CONTRACT)
                                        .trim()
                                        .to_string();
                                    let mut expected_paths: Vec<String> = Vec::new();
                                    for ds in ids.iter() {
                                        if let Some(t) =
                                            plan.tasks.iter().find(|t| t.dataset_id == *ds)
                                        {
                                            if let Some(p) = t.expected_model_path.as_deref() {
                                                if !p.trim().is_empty() {
                                                    expected_paths.push(p.trim().to_string());
                                                }
                                            }
                                        }
                                    }
                                    expected_paths.sort();
                                    expected_paths.dedup();
                                    let mut ctx = format!(
                                        "Approved cleanse plan (stored at: {}).\nPending schema checklist work (checklist_item_id={} ; max 5):\n- {}\n\nNext action: call apply_next_cleanse_schema_batch (do NOT call dbt_files directly).\n\nExpected model SQL paths:\n- {}\n",
                                        plan.plan_key,
                                        checklist_item_id,
                                        ids.join("\n- "),
                                        expected_paths.join("\n- "),
                                    );
                                    ctx.push_str("\nIMPORTANT: Do NOT call apply_next_cleanse_batch while schema checklist work remains; that tool only authors SQL.\n");
                                    (ctx, None)
                                } else {
                                    if let Some((
                                        crate::data_engineer::plan::WorkGroupKind::Validate,
                                        _ids,
                                    )) = next_action.as_ref()
                                    {
                                        control_flow::append_phase_with_reason(
                                            &thread_store,
                                            thread_id,
                                            Some("agent".to_string()),
                                            Some(phase),
                                            Phase::CleanseValidate,
                                            Some("work_group_validate"),
                                            Some(serde_json::json!({ "plan_key": plan.plan_key })),
                                        )
                                        .await;
                                        continue;
                                    }

                                    // If validation previously failed, do NOT bounce straight back to validate.
                                    // Run a repair authoring pass grounded in the failing model/file evidence.
                                    if guard.last_validate_failed {
                                        let mut ctx = format!(
                                        "Approved cleanse plan (stored at: {}).\nAll plan tasks are currently marked done, but the last dbt_validate failed.\n\nRepair targets (fix these DBT files directly with dbt_files op=patch using replace_file/replace_range/replace_list):\n",
                                        plan.plan_key
                                    );
                                        if !last_validate_failed_models.is_empty() {
                                            for fm in last_validate_failed_models.iter().take(6) {
                                                let name = fm
                                                    .get("name")
                                                    .and_then(|v| v.as_str())
                                                    .unwrap_or("unknown_model");
                                                let file = fm
                                                    .get("file")
                                                    .and_then(|v| v.as_str())
                                                    .unwrap_or("(unknown file)");
                                                ctx.push_str(&format!("- {} ({})\n", name, file));
                                            }
                                        } else if let Some(ref brief) = last_validate_brief {
                                            ctx.push_str("- (unknown failing model) — see last dbt_validate summary below.\n");
                                            ctx.push_str("\nLast dbt_validate summary:\n");
                                            ctx.push_str(brief);
                                            ctx.push('\n');
                                        } else {
                                            ctx.push_str("- (unknown failing model) — no failing-model evidence found.\n");
                                        }
                                        (
                                            ctx,
                                            None, // allow freeform dbt_files patching for targeted repair
                                        )
                                    } else {
                                        // If schema checklist work remains (legacy plans without work_groups),
                                        // stay in authoring and request YAML patching.
                                        let pending_schema =
                                            crate::data_engineer::plan::cleanse_pending_for_checklist(
                                                &plan,
                                                crate::data_engineer::plan::CHECKLIST_SQL_MODEL,
                                                crate::data_engineer::plan::CHECKLIST_SCHEMA_CONTRACT,
                                            );
                                        if !pending_schema.is_empty() {
                                            let checklist_item_id = crate::data_engineer::plan::CHECKLIST_SCHEMA_CONTRACT;
                                            let mut expected_paths: Vec<String> = Vec::new();
                                            for ds in pending_schema.iter() {
                                                if let Some(t) =
                                                    plan.tasks.iter().find(|t| t.dataset_id == *ds)
                                                {
                                                    if let Some(p) = t.expected_model_path.as_deref() {
                                                        if !p.trim().is_empty() {
                                                            expected_paths.push(p.trim().to_string());
                                                        }
                                                    }
                                                }
                                            }
                                            expected_paths.sort();
                                            expected_paths.dedup();
                                            let mut ctx = format!(
                                                "Approved cleanse plan (stored at: {}).\nPending schema checklist work (checklist_item_id={} ; max 5):\n- {}\n\nNext action: call apply_next_cleanse_schema_batch (do NOT call dbt_files directly).\n\nExpected model SQL paths:\n- {}\n",
                                                plan.plan_key,
                                                checklist_item_id,
                                                pending_schema.join("\n- "),
                                                expected_paths.join("\n- "),
                                            );
                                            ctx.push_str("\nIMPORTANT: Do NOT call apply_next_cleanse_batch while schema checklist work remains; that tool only authors SQL.\n");
                                            (ctx, None)
                                        } else {
                                            // All SQL + schema tasks are done; advance to validate.
                                            control_flow::append_phase_with_reason(
                                                &thread_store,
                                                thread_id,
                                                Some("agent".to_string()),
                                                Some(phase),
                                                Phase::CleanseValidate,
                                                Some("plan_tasks_done"),
                                                Some(serde_json::json!({ "plan_key": plan.plan_key })),
                                            )
                                            .await;
                                            continue;
                                        }
                                    }
                                }
                            } else {
                                (
                                format!(
								"Approved cleanse plan (stored at: {}).\nNext batch (deterministic, max 5):\n- {}\n\nNext action: call apply_next_cleanse_batch (do NOT call staging_model directly).",
                                plan.plan_key,
                                next.join("\n- ")
                                ),
                                Some(AllowedBatch::CleanseDatasetIds(next.clone())),
                            )
                            }
                        } else {
                            let mut plan = match crate::data_engineer::plan::load_model_plan_any(&actx)
                                .await
                            {
                                Some(p) => p,
                                None => {
                                    return Err("missing active model plan in authoring phase (expected plan to exist)".to_string());
                                }
                            };
                            // Safety: do not allow authoring to proceed with an empty/invalid approved plan,
                            // otherwise the phase will fast-forward (plan_tasks_done) without doing work.
                            if plan.tasks.is_empty() || plan.batches.is_empty() {
                                let plan_key = plan.plan_key.clone();
                                plan.status = crate::data_engineer::plan::PlanStatus::Cancelled;
                                let _ =
                                    crate::data_engineer::plan::save_model_plan(&actx, &plan).await;
                                control_flow::append_phase_with_reason(
                                    &thread_store,
                                    thread_id,
                                    Some("agent".to_string()),
                                    Some(phase),
                                    Phase::ModelPlan,
                                    Some("plan_invalid_empty"),
                                    Some(serde_json::json!({
                                        "plan_key": plan_key,
                                        "tasks_len": plan.tasks.len(),
                                        "batches_len": plan.batches.len(),
                                    })),
                                )
                                .await;
                                continue;
                            }
                            if let Some(ref l) = log {
                                crate::data_engineer::plan::update_model_progress_from_log(
                                    &mut plan, l,
                                );
                                let _ =
                                    crate::data_engineer::plan::save_model_plan(&actx, &plan).await;
                            }

                            // Explicit execution context for hierarchical UI (best-effort).
                            let next_item =
                                crate::data_engineer::plan::model_next_work_item_ctx(&plan);
                            actx.exec_ctx = Some(react_core::session::ExecutionContext {
                                plan_kind: Some("model".to_string()),
                                plan_key: Some(plan.plan_key.clone()),
                                workgroup_id: next_item.as_ref().map(|x| x.workgroup_id.clone()),
                                task_id: next_item.as_ref().map(|x| x.task_id.clone()),
                                checklist_item_id: next_item
                                    .as_ref()
                                    .map(|x| x.checklist_item_id.clone()),
                            });
                            if plan.progress.consecutive_batch_failures
                            >= crate::data_engineer::tools::apply_next_batch::MAX_CONSECUTIVE_BATCH_FAILURES
                        {
                            let next = match crate::data_engineer::plan::model_next_action(&plan) {
                                Some((crate::data_engineer::plan::WorkGroupKind::AuthorSql, names)) => {
                                    names
                                }
                                _ => crate::data_engineer::plan::model_next_batch(&plan),
                            };
                            let mut expected_paths: Vec<String> = Vec::new();
                            for n in next.iter() {
                                if let Some(t) = plan.tasks.iter().find(|t| t.name == *n) {
                                    if let Some(p) = t.expected_model_path.as_deref() {
                                        if !p.trim().is_empty() {
                                            expected_paths.push(p.trim().to_string());
                                        }
                                    }
                                }
                            }
                            expected_paths.sort();
                            expected_paths.dedup();
                            let reason = lock_prompt_for_plan(
                                "model",
                                &plan.plan_key,
                                plan.progress.consecutive_batch_failures,
                                plan.progress.total_batch_failures,
                                &next,
                                &expected_paths,
                            );
                            let ts = chrono::Utc::now().to_rfc3339();
                            let step = react_core::session::ThreadStep::GuardBlock {
                                phase: phase.as_str().to_string(),
                                kind: "batch_locked".to_string(),
                                reason: reason.clone(),
                                observation: react_core::session::Observation::fail(vec![reason.clone()]),
                                ts,
                                agent: "agent".to_string(),
                            };
                            let _ = thread_store.append_step(thread_id, step).await;
                            return Ok(vec![FlowFrame::AwaitUser { prompt: reason }]);
                        }
                            if plan.status != crate::data_engineer::plan::PlanStatus::Approved
                                && plan.status != crate::data_engineer::plan::PlanStatus::Completed
                            {
                                control_flow::append_phase_with_reason(
                                &thread_store,
                                thread_id,
                                Some("agent".to_string()),
                                Some(phase),
                                Phase::ModelPlan,
                                Some("plan_not_approved"),
                                Some(serde_json::json!({ "status": format!("{:?}", plan.status) })),
                            )
                            .await;
                                continue;
                            }
                            // Work-group driven selection (preferred). Fall back to batch scanning if work_groups
                            // are absent (older plans).
                            let next_action = crate::data_engineer::plan::model_next_action(&plan);
                            let next_names = match next_action.as_ref() {
                                Some((crate::data_engineer::plan::WorkGroupKind::AuthorSql, names)) => {
                                    names.clone()
                                }
                                _ => crate::data_engineer::plan::model_next_batch(&plan),
                            };
                            // IMPORTANT: If validation failed and we have not successfully mutated since,
                            // the authoring tool registry will be patch-only (hard_mutation_only).
                            // In that state, do NOT instruct apply_next_model_batch; force repair-mode guidance.
                            if hard_mutation_repair_mode && !prefer_schema_repairs {
                                // Repair-first routing: dbt_validate failed for a SQL/runtime-class reason.
                                // Even if schema checklist work remains, fix failing SQL targets first.
                                let mut ctx = format!(
                                    "Approved model plan (stored at: {}).\nThe last dbt_validate failed and a mutating fix is required before any further validation.\n\nNext action: call dbt_files op=patch (replace_file/replace_range/replace_list) to fix the failing SQL target(s) below. Keep changes minimal.\n\nRepair targets:\n",
                                    plan.plan_key
                                );
                                if !last_validate_failed_models.is_empty() {
                                    for fm in last_validate_failed_models.iter().take(6) {
                                        let name = fm
                                            .get("name")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("unknown_model");
                                        let file = fm
                                            .get("file")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("(unknown file)");
                                        ctx.push_str(&format!("- {} ({})\n", name, file));
                                    }
                                } else if let Some(ref brief) = last_validate_brief {
                                    ctx.push_str("- (unknown failing model) — see last dbt_validate summary below.\n");
                                    ctx.push_str("\nLast dbt_validate summary:\n");
                                    ctx.push_str(brief);
                                    ctx.push('\n');
                                } else {
                                    ctx.push_str("- (unknown failing model) — no failing-model evidence found.\n");
                                }
                                ctx.push_str("\nIMPORTANT: Defer any new checklist expansion or schema contract work until dbt_validate passes.\n");
                                (ctx, None)
                            } else if hard_mutation_repair_mode {
                                if let Some((
                                    crate::data_engineer::plan::WorkGroupKind::AuthorSchema,
                                    ids,
                                )) = next_action.as_ref()
                                {
                                    let checklist_item_id = actx
                                        .exec_ctx
                                        .as_ref()
                                        .and_then(|c| c.checklist_item_id.as_deref())
                                        .unwrap_or(crate::data_engineer::plan::CHECKLIST_SCHEMA_CONTRACT)
                                        .trim()
                                        .to_string();
                                    let mut expected_paths: Vec<String> = Vec::new();
                                    for n in ids.iter() {
                                        if let Some(t) = plan.tasks.iter().find(|t| t.name == *n) {
                                            if let Some(p) = t.expected_model_path.as_deref() {
                                                if !p.trim().is_empty() {
                                                    expected_paths.push(p.trim().to_string());
                                                }
                                            }
                                        }
                                    }
                                    expected_paths.sort();
                                    expected_paths.dedup();
                                    let mut ctx = format!(
                                        "Approved model plan (stored at: {}).\nPending schema checklist work (checklist_item_id={} ; max 5):\n- {}\n\nNext action: call apply_next_model_schema_batch (do NOT call dbt_files directly).\n\nExpected model SQL paths:\n- {}\n",
                                        plan.plan_key,
                                        checklist_item_id,
                                        ids.join("\n- "),
                                        expected_paths.join("\n- "),
                                    );
                                    ctx.push_str("\nIMPORTANT: Do NOT call apply_next_model_batch while schema checklist work remains; that tool only authors SQL.\n");
                                    (ctx, None)
                                } else {
                                    let pending_schema =
                                        crate::data_engineer::plan::model_pending_for_checklist(
                                            &plan,
                                            crate::data_engineer::plan::CHECKLIST_SQL_MODEL,
                                            crate::data_engineer::plan::CHECKLIST_SCHEMA_CONTRACT,
                                        );
                                    if !pending_schema.is_empty() {
                                        let checklist_item_id = crate::data_engineer::plan::CHECKLIST_SCHEMA_CONTRACT;
                                        let mut expected_paths: Vec<String> = Vec::new();
                                        for n in pending_schema.iter() {
                                            if let Some(t) = plan.tasks.iter().find(|t| t.name == *n) {
                                                if let Some(p) = t.expected_model_path.as_deref() {
                                                    if !p.trim().is_empty() {
                                                        expected_paths.push(p.trim().to_string());
                                                    }
                                                }
                                            }
                                        }
                                        expected_paths.sort();
                                        expected_paths.dedup();
                                        let mut ctx = format!(
                                            "Approved model plan (stored at: {}).\nPending schema checklist work (checklist_item_id={} ; max 5):\n- {}\n\nNext action: call apply_next_model_schema_batch (do NOT call dbt_files directly).\n\nExpected model SQL paths:\n- {}\n",
                                            plan.plan_key,
                                            checklist_item_id,
                                            pending_schema.join("\n- "),
                                            expected_paths.join("\n- "),
                                        );
                                        ctx.push_str("\nIMPORTANT: Do NOT call apply_next_model_batch while schema checklist work remains; that tool only authors SQL.\n");
                                        (ctx, None)
                                    } else {
                                let mut ctx = format!(
                                "Approved model plan (stored at: {}).\nThe last dbt_validate failed and a mutating fix is required before any further validation.\n\nRepair targets (fix these DBT files directly with dbt_files op=patch using replace_file/replace_range/replace_list):\n",
                                plan.plan_key
                            );
                                if !last_validate_failed_models.is_empty() {
                                    for fm in last_validate_failed_models.iter().take(6) {
                                        let name = fm
                                            .get("name")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("unknown_model");
                                        let file = fm
                                            .get("file")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("(unknown file)");
                                        ctx.push_str(&format!("- {} ({})\n", name, file));
                                    }
                                } else if let Some(ref brief) = last_validate_brief {
                                    ctx.push_str("- (unknown failing model) — see last dbt_validate summary below.\n");
                                    ctx.push_str("\nLast dbt_validate summary:\n");
                                    ctx.push_str(brief);
                                    ctx.push('\n');
                                } else {
                                    ctx.push_str("- (unknown failing model) — no failing-model evidence found.\n");
                                }
                                (ctx, None)
                                    }
                                }
                            } else if next_names.is_empty() {
                                if let Some((
                                    crate::data_engineer::plan::WorkGroupKind::AuthorSchema,
                                    ids,
                                )) = next_action.as_ref()
                                {
                                    // Deterministic pre-check: if models/schema.yml already contains model stanzas
                                    // for these pending items, mark schema_contract done and re-run planning for the
                                    // next action instead of thrashing the same file.
                                    {
                                        let key = crate::data_engineer::project_fs::join_storage_key(
                                            &actx,
                                            crate::data_engineer::project_files::MODELS_SCHEMA_YML,
                                        );
                                        if let Ok(bytes) = actx.storage.get_bytes(&key).await {
                                            let content =
                                                String::from_utf8_lossy(&bytes).to_string();
                                            if let Ok(vy) =
                                                serde_yaml::from_str::<serde_yaml::Value>(&content)
                                            {
                                                let mut names_in_schema: std::collections::HashSet<String> =
                                                    std::collections::HashSet::new();
                                                if let Some(models) = vy
                                                    .get("models")
                                                    .and_then(|m| m.as_sequence())
                                                {
                                                    for m in models.iter() {
                                                        if let Some(nm) = m
                                                            .get("name")
                                                            .and_then(|n| n.as_str())
                                                            .map(|s| s.trim().to_string())
                                                            .filter(|s| !s.is_empty())
                                                        {
                                                            names_in_schema.insert(nm);
                                                        }
                                                    }
                                                }
                                                let mut changed = false;
                                                let checklist_item_id = actx
                                                    .exec_ctx
                                                    .as_ref()
                                                    .and_then(|c| c.checklist_item_id.as_deref())
                                                    .unwrap_or(
                                                        crate::data_engineer::plan::CHECKLIST_SCHEMA_CONTRACT,
                                                    )
                                                    .trim()
                                                    .to_string();
                                                for n in ids.iter() {
                                                    if !names_in_schema.contains(n) {
                                                        continue;
                                                    }
                                                    if let Some(t) =
                                                        plan.tasks.iter().find(|t| t.name == *n)
                                                    {
                                                        let done = t
                                                            .checklist
                                                            .iter()
                                                            .find(|it| it.checklist_item_id == checklist_item_id)
                                                            .map(|it| {
                                                                it.status
                                                                    == crate::data_engineer::plan::ChecklistItemStatus::Done
                                                            })
                                                            .unwrap_or(false);
                                                        if !done {
                                                            changed = true;
                                                        }
                                                    }
                                                    crate::data_engineer::plan::model_checklist_mark_status(
                                                        &mut plan,
                                                        n,
                                                        &checklist_item_id,
                                                        crate::data_engineer::plan::ChecklistItemStatus::Done,
                                                    );
                                                }
                                                if changed {
                                                    crate::data_engineer::plan::save_model_plan(
                                                        &actx,
                                                        &plan,
                                                    )
                                                    .await?;
                                                    continue;
                                                }
                                            }
                                        }
                                    }

                                    let mut expected_paths: Vec<String> = Vec::new();
                                    for n in ids.iter() {
                                        if let Some(t) = plan.tasks.iter().find(|t| t.name == *n) {
                                            if let Some(p) = t.expected_model_path.as_deref() {
                                                if !p.trim().is_empty() {
                                                    expected_paths.push(p.trim().to_string());
                                                }
                                            }
                                        }
                                    }
                                    expected_paths.sort();
                                    expected_paths.dedup();
                                    let checklist_item_id = actx
                                        .exec_ctx
                                        .as_ref()
                                        .and_then(|c| c.checklist_item_id.as_deref())
                                        .unwrap_or(crate::data_engineer::plan::CHECKLIST_SCHEMA_CONTRACT)
                                        .trim()
                                        .to_string();
                                    let mut ctx = format!(
                                        "Approved model plan (stored at: {}).\nPending schema checklist work (checklist_item_id={} ; max 5):\n- {}\n\nNext action: call apply_next_model_schema_batch (do NOT call dbt_files directly).\n\nExpected model SQL paths:\n- {}\n",
                                        plan.plan_key,
                                        checklist_item_id,
                                        ids.join("\n- "),
                                        expected_paths.join("\n- "),
                                    );
                                    ctx.push_str("\nIMPORTANT: Do NOT call apply_next_model_batch while schema checklist work remains; that tool only authors SQL.\n");
                                    (ctx, None)
                                } else {
                                    if let Some((
                                        crate::data_engineer::plan::WorkGroupKind::Validate,
                                        _ids,
                                    )) = next_action.as_ref()
                                    {
                                        control_flow::append_phase_with_reason(
                                            &thread_store,
                                            thread_id,
                                            Some("agent".to_string()),
                                            Some(phase),
                                            Phase::ModelValidate,
                                            Some("work_group_validate"),
                                            Some(serde_json::json!({ "plan_key": plan.plan_key })),
                                        )
                                        .await;
                                        continue;
                                    }

                                    if guard.last_validate_failed {
                                    // Same repair-mode behavior as cleanse: run authoring to patch failing files.
                                    let mut ctx = format!(
                                    "Approved model plan (stored at: {}).\nAll plan tasks are currently marked done, but the last dbt_validate failed.\n\nRepair targets (fix these DBT files directly with dbt_files op=patch using replace_file/replace_range/replace_list):\n",
                                    plan.plan_key
                                );
                                    if !last_validate_failed_models.is_empty() {
                                        for fm in last_validate_failed_models.iter().take(6) {
                                            let name = fm
                                                .get("name")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("unknown_model");
                                            let file = fm
                                                .get("file")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("(unknown file)");
                                            ctx.push_str(&format!("- {} ({})\n", name, file));
                                        }
                                    } else if let Some(ref brief) = last_validate_brief {
                                        ctx.push_str("- (unknown failing model) — see last dbt_validate summary below.\n");
                                        ctx.push_str("\nLast dbt_validate summary:\n");
                                        ctx.push_str(brief);
                                        ctx.push('\n');
                                    } else {
                                        ctx.push_str("- (unknown failing model) — no failing-model evidence found.\n");
                                    }
                                    (ctx, None)
                                } else {
                                    let pending_schema =
                                        crate::data_engineer::plan::model_pending_for_checklist(
                                            &plan,
                                            crate::data_engineer::plan::CHECKLIST_SQL_MODEL,
                                            crate::data_engineer::plan::CHECKLIST_SCHEMA_CONTRACT,
                                        );
                                    if !pending_schema.is_empty() {
                                        // Deterministic pre-check: if models/schema.yml already contains these
                                        // models, mark the current schema checklist item done and restart.
                                        {
                                            let key =
                                                crate::data_engineer::project_fs::join_storage_key(
                                                    &actx,
                                                    crate::data_engineer::project_files::MODELS_SCHEMA_YML,
                                                );
                                            if let Ok(bytes) = actx.storage.get_bytes(&key).await {
                                                let content =
                                                    String::from_utf8_lossy(&bytes).to_string();
                                                if let Ok(vy) = serde_yaml::from_str::<
                                                    serde_yaml::Value,
                                                >(&content)
                                                {
                                                    let mut names_in_schema: std::collections::HashSet<String> =
                                                        std::collections::HashSet::new();
                                                    if let Some(models) = vy
                                                        .get("models")
                                                        .and_then(|m| m.as_sequence())
                                                    {
                                                        for m in models.iter() {
                                                            if let Some(nm) = m
                                                                .get("name")
                                                                .and_then(|n| n.as_str())
                                                                .map(|s| s.trim().to_string())
                                                                .filter(|s| !s.is_empty())
                                                            {
                                                                names_in_schema.insert(nm);
                                                            }
                                                        }
                                                    }
                                                    let mut changed = false;
                                                    let checklist_item_id = actx
                                                        .exec_ctx
                                                        .as_ref()
                                                        .and_then(|c| c.checklist_item_id.as_deref())
                                                        .unwrap_or(
                                                            crate::data_engineer::plan::CHECKLIST_SCHEMA_CONTRACT,
                                                        )
                                                        .trim()
                                                        .to_string();
                                                    for n in pending_schema.iter() {
                                                        if !names_in_schema.contains(n) {
                                                            continue;
                                                        }
                                                        if let Some(t) = plan
                                                            .tasks
                                                            .iter()
                                                            .find(|t| t.name == *n)
                                                        {
                                                            let done = t
                                                                .checklist
                                                                .iter()
                                                                .find(|it| it.checklist_item_id == checklist_item_id)
                                                                .map(|it| {
                                                                    it.status
                                                                        == crate::data_engineer::plan::ChecklistItemStatus::Done
                                                                })
                                                                .unwrap_or(false);
                                                            if !done {
                                                                changed = true;
                                                            }
                                                        }
                                                        crate::data_engineer::plan::model_checklist_mark_status(
                                                            &mut plan,
                                                            n,
                                                            &checklist_item_id,
                                                            crate::data_engineer::plan::ChecklistItemStatus::Done,
                                                        );
                                                    }
                                                    if changed {
                                                        crate::data_engineer::plan::save_model_plan(
                                                            &actx,
                                                            &plan,
                                                        )
                                                        .await?;
                                                        continue;
                                                    }
                                                }
                                            }
                                        }

                                        let mut expected_paths: Vec<String> = Vec::new();
                                        for n in pending_schema.iter() {
                                            if let Some(t) = plan.tasks.iter().find(|t| t.name == *n) {
                                                if let Some(p) = t.expected_model_path.as_deref() {
                                                    if !p.trim().is_empty() {
                                                        expected_paths.push(p.trim().to_string());
                                                    }
                                                }
                                            }
                                        }
                                        expected_paths.sort();
                                        expected_paths.dedup();
                                        let checklist_item_id = actx
                                            .exec_ctx
                                            .as_ref()
                                            .and_then(|c| c.checklist_item_id.as_deref())
                                            .unwrap_or(crate::data_engineer::plan::CHECKLIST_SCHEMA_CONTRACT)
                                            .trim()
                                            .to_string();
                                        let mut ctx = format!(
                                            "Approved model plan (stored at: {}).\nPending schema checklist work (checklist_item_id={} ; max 5):\n- {}\n\nNext action: call apply_next_model_schema_batch (do NOT call dbt_files directly).\n\nExpected model SQL paths:\n- {}\n",
                                            plan.plan_key,
                                            checklist_item_id,
                                            pending_schema.join("\n- "),
                                            expected_paths.join("\n- "),
                                        );
                                        ctx.push_str("\nIMPORTANT: Do NOT call apply_next_model_batch while schema checklist work remains; that tool only authors SQL.\n");
                                        (ctx, None)
                                    } else {
                                        control_flow::append_phase_with_reason(
                                            &thread_store,
                                            thread_id,
                                            Some("agent".to_string()),
                                            Some(phase),
                                            Phase::ModelValidate,
                                            Some("plan_tasks_done"),
                                            Some(serde_json::json!({ "plan_key": plan.plan_key })),
                                        )
                                        .await;
                                        continue;
                                    }
                                }
                                }
                            } else {
                                let allowed =
                                    Some(AllowedBatch::ModelItemNames(next_names.clone()));
                                // Include task details for the next batch so the LLM can call gold_model with full args.
                                let mut details: Vec<String> = Vec::new();
                                for n in next_names.iter() {
                                    if let Some(t) = plan.tasks.iter().find(|t| t.name == *n) {
                                        details.push(format!(
                                            "- name: {}\n  folder: {}\n  goal: {}\n  inputs: {:?}",
                                            t.name, t.folder, t.goal, t.inputs
                                        ));
                                    } else {
                                        details.push(format!("- name: {}", n));
                                    }
                                }
                                (
                                format!(
								"Approved model plan (stored at: {}).\nNext batch (deterministic, max 5):\n{}\n\nNext action: call apply_next_model_batch (do NOT call gold_model directly).",
                                plan.plan_key,
                                details.join("\n")
                                ),
                                allowed,
                            )
                            }
                        };

                    let (registry, tools_card) = Self::build_tools_for_phase(
                        phase,
                        &phase_guard,
                        allow_ask_approval,
                        sctx,
                        allowed_batch.clone(),
                    )?;

                    // Ground the next authoring pass with last validation summary (if any) and guard state.
                    let mut q = if is_cleanse {
                        Self::inject_cleanse_question(question)
                    } else {
                        Self::inject_model_question(question)
                    };
                    if !is_cleanse {
                        // Ground gold authoring with the current silver inventory so the agent
                        // can reliably build marts from existing stg_* models (no guessing).
                        let base = actx
                            .keyspace
                            .dbt_prefix(&actx.scope)
                            .trim_end_matches('/')
                            .to_string();
                        let pref = format!("{}/models/staging/", base);
                        if let Ok(keys) = actx.storage.list_prefix(&pref).await {
                            let mut rels: Vec<String> = keys
                                .into_iter()
                                .filter(|k| k.ends_with(".sql") && !k.contains("/_versions/"))
                                .filter_map(|k| {
                                    k.strip_prefix(&(base.clone() + "/")).map(|s| s.to_string())
                                })
                                .collect();
                            rels.sort();
                            rels.dedup();
                            let mut names: Vec<String> = rels
                                .into_iter()
                                .filter_map(|rel| {
                                    std::path::Path::new(&rel)
                                        .file_stem()
                                        .map(|s| s.to_string_lossy().to_string())
                                })
                                .collect();
                            names.sort();
                            names.dedup();
                            if !names.is_empty() {
                                q.push_str("\n\nCurrent staged silver models (use ref('stg_*') from these):\n");
                                for n in names.into_iter().take(60) {
                                    q.push_str("- ");
                                    q.push_str(&n);
                                    q.push('\n');
                                }
                            }
                        }
                    }
                    q.push_str("\n\nNOTE: In agent mode, validation and publish are handled by the suite phases. Do not call dbt_validate or publish tools; focus on authoring fixes and models.");
                    q.push_str("\nIMPORTANT: Tool-call argument shapes are strict. In particular: vect_query uses args.query_text (NOT args.query) and scope must be \"dataset\"|\"field\"|\"doc\"|\"artifact\"|\"metric\"|\"model\".");
                    q.push_str("\nIMPORTANT: sql_stats and sql_sample both require args.field. To sample rows, use run_sql with a LIMIT.");
                    q.push_str("\nIMPORTANT: This authoring phase is plan-driven. Follow the Plan context below. If it says to patch failing DBT files, do that first; if it provides a next batch, execute it. Do NOT ask for approval; approvals happen in plan phases.");
                    q.push_str("\n\nPlan context:\n");
                    q.push_str(&plan_context);
                    // Auto-attach authoritative schema facts (no ambiguity) for this phase.
                    // - In authoring, include ALL relations in the current approved batch.
                    // - Also include any recent validate-fail facts snapshot if present.
                    {
                        let dialect = crate::config::resolved_config_from_ctx(&actx)
                            .as_ref()
                            .map(|cfg| crate::data_engineer::dbt_repair::remediate::active_provider_dialect(cfg))
                            .unwrap_or_else(|| "Unknown SQL dialect".to_string());
                        let mut batch_relations: Vec<String> = Vec::new();
                        let mut prior_validate_facts: Option<serde_json::Value> = None;
                        if is_cleanse {
                            if let Some(p) =
                                crate::data_engineer::plan::load_cleanse_plan(&actx).await
                            {
                                if let Some(ab) = allowed_batch.as_ref() {
                                    if let AllowedBatch::CleanseDatasetIds(ds) = ab {
                                        batch_relations =
                                            crate::data_engineer::facts::dataset_ids_to_fqns(ds);
                                    }
                                }
                                // Prefer the last persisted validate_fail_facts snapshot (if any).
                                if let Some(obj) = p.project_snapshot.as_object() {
                                    if let Some(arr) =
                                        obj.get("validate_fail_facts").and_then(|v| v.as_array())
                                    {
                                        if let Some(last) = arr.last() {
                                            prior_validate_facts = Some(last.clone());
                                        }
                                    }
                                }
                            }
                        } else {
                            if let Some(p) =
                                crate::data_engineer::plan::load_model_plan(&actx).await
                            {
                                if let Some(ab) = allowed_batch.as_ref() {
                                    if let AllowedBatch::ModelItemNames(names) = ab {
                                        // Include relations for the models in the batch AND their declared inputs.
                                        let mut want_names: Vec<String> = names.clone();
                                        for n in names.iter() {
                                            if let Some(t) = p.tasks.iter().find(|t| t.name == *n) {
                                                for inp in t.inputs.iter() {
                                                    let s = inp.trim();
                                                    if !s.is_empty() {
                                                        want_names.push(s.to_string());
                                                    }
                                                }
                                            }
                                        }
                                        want_names.sort();
                                        want_names.dedup();
                                        batch_relations =
                                            crate::data_engineer::facts::resolve_model_names_to_fqns(&actx, &want_names).await;
                                    }
                                }
                                if let Some(obj) = p.project_snapshot.as_object() {
                                    if let Some(arr) =
                                        obj.get("validate_fail_facts").and_then(|v| v.as_array())
                                    {
                                        if let Some(last) = arr.last() {
                                            prior_validate_facts = Some(last.clone());
                                        }
                                    }
                                }
                            }
                        }

                        if !batch_relations.is_empty() {
                            let limits = crate::data_engineer::facts::FactsLimits::for_scope(
                                crate::data_engineer::facts::FactsScope::AuthorBatch,
                            );
                            let bundle =
                                crate::data_engineer::facts::build_facts_bundle_from_relations(
                                    &actx,
                                    crate::data_engineer::facts::FactsScope::AuthorBatch,
                                    dialect.clone(),
                                    crate::data_engineer::facts::TargetFacts::default(),
                                    &batch_relations,
                                    limits,
                                )
                                .await;
                            q.push_str("\n\nIMMUTABLE FACTS (author_batch_schema):\n");
                            q.push_str(
                                &serde_json::to_string_pretty(&bundle)
                                    .unwrap_or_else(|_| "{}".to_string()),
                            );
                            q.push('\n');
                            q.push_str("Rules:\n- You MUST NOT reference any column not present in facts.relations[].columns for that relation.\n- If required facts are missing, call sql_schema and then patch.\n");
                        }
                        if let Some(vf) = prior_validate_facts {
                            q.push_str("\n\nIMMUTABLE FACTS (latest_validate_fail_facts):\n");
                            q.push_str(
                                &serde_json::to_string_pretty(&vf)
                                    .unwrap_or_else(|_| "{}".to_string()),
                            );
                            q.push('\n');
                        }
                    }
                    if let Some(b) = last_validate_brief.as_ref() {
                        q.push_str("\n\nLast dbt_validate summary (most recent):\n");
                        q.push_str(b);
                    }
                    if !last_validate_failed_models.is_empty() {
                        q.push_str("\n\nFailing DBT model targets (from dbt stdout):\n");
                        for fm in last_validate_failed_models.iter().take(6) {
                            let name = fm
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown_model");
                            let file = fm
                                .get("file")
                                .and_then(|v| v.as_str())
                                .unwrap_or("(unknown file)");
                            q.push_str("- ");
                            q.push_str(name);
                            q.push_str(" (");
                            q.push_str(file);
                            q.push_str(")\n");
                        }
                        q.push_str("Fix these first (prefer patching the listed file paths).\n");
                    }

                    // When the suite is in hard_mutation_only, dbt_files is patch-only (no op=get),
                    // so we MUST include the raw file content for at least the primary failing target.
                    if hard_mutation_repair_mode && !last_validate_failed_models.is_empty()
                    {
                        if let Some(file) = last_validate_failed_models[0]
                            .get("file")
                            .and_then(|v| v.as_str())
                        {
                            let file = file.trim();
                            if !file.is_empty() && file != "(unknown file)" {
                                let base = actx
                                    .keyspace
                                    .dbt_prefix(&actx.scope)
                                    .trim_end_matches('/')
                                    .to_string();
                                let key = format!("{}/{}", base, file);
                                if let Ok(bytes) = actx.storage.get_bytes(&key).await {
                                    let content = String::from_utf8_lossy(&bytes).to_string();
                                    q.push_str("\n\nPrimary repair target current file content:\n");
                                    q.push_str("File: ");
                                    q.push_str(file);
                                    q.push_str("\n\n```sql\n");
                                    q.push_str(&content);
                                    if !content.ends_with('\n') {
                                        q.push('\n');
                                    }
                                    q.push_str("```\n");
                                }
                            }
                        }
                    }
                    // Surface the most recent suite-level guard block reason (if any) to help auto-fix.
                    if let Some(ref l) = log {
                        if let Some(reason) = l.steps.iter().rev().find_map(|s| match s {
                            react_core::session::ThreadStep::GuardBlock { reason, .. } => {
                                Some(reason.as_str())
                            }
                            _ => None,
                        }) {
                            if !reason.trim().is_empty() {
                                q.push_str(
                                    "\n\nSuite guard note (must resolve before validate):\n",
                                );
                                q.push_str(reason.trim());
                            }
                        }
                    }
                    if hard_mutation_repair_mode {
                        q.push_str("\n\nConstraint: your next steps must APPLY A MUTATING FIX before attempting dbt_validate again.");
                    }
                    if phase_guard.probe_required && !phase_guard.probe_satisfied {
                        q.push_str("\n\nConstraint: runtime validation failed after compile; you MUST run meaningful run_sql probes (not SELECT 1) to diagnose data before re-validating.");
                    }

                    // Deterministic invariant: do not allow leaving authoring without any models.
                    let has_models = control_flow::invariant_has_any_models(&actx)
                        .await
                        .unwrap_or(false);
                    if !has_models {
                        q.push_str("\n\nIMPORTANT: invariant failed: there are no DBT model SQL files yet. Your first task is to create at least one staging model under models/ using staging_model or dbt_files op=patch.");
                    }

                    let llm_options = if is_cleanse {
                        Some(LlmCallOptions {
                            temperature: Some(0.05),
                            top_p: Some(1.0),
                            max_output_tokens: Some(1800),
                        })
                    } else {
                        Some(LlmCallOptions {
                            temperature: Some(0.12),
                            top_p: Some(1.0),
                            max_output_tokens: Some(2200),
                        })
                    };
                    match Agent::run_until_block(&registry, &actx, &sys, &tools_card, &q, llm_options).await {
                        Ok(RunOutcome::Final { .. }) => {
                            // Update plan progress based on newly recorded tool steps.
                            if let Ok(latest) = thread_store.get(thread_id).await {
                                if is_cleanse {
                                    if let Some(mut p) =
                                        crate::data_engineer::plan::load_cleanse_plan(&actx).await
                                    {
                                        crate::data_engineer::plan::update_cleanse_progress_from_log(&mut p, &latest);
                                        let _ = crate::data_engineer::plan::save_cleanse_plan(
                                            &actx, &p,
                                        )
                                        .await;
                                    }
                                } else {
                                    if let Some(mut p) =
                                        crate::data_engineer::plan::load_model_plan(&actx).await
                                    {
                                        crate::data_engineer::plan::update_model_progress_from_log(
                                            &mut p, &latest,
                                        );
                                        let _ =
                                            crate::data_engineer::plan::save_model_plan(&actx, &p)
                                                .await;
                                    }
                                }
                            }

                            // Deterministic invariants: don't advance phases unless the project actually exists.
                            let has_proj = control_flow::invariant_has_dbt_project(&actx)
                                .await
                                .unwrap_or(false);
                            let has_models = control_flow::invariant_has_any_models(&actx)
                                .await
                                .unwrap_or(false);
                            if !has_proj || !has_models {
                                // Stay in the same authoring phase; the next pass will be prompted with invariant context.
                                continue;
                            }
                            // New guard: do not advance if this authoring phase has unresolved mutation/tool failures.
                            // Auto-loop in authoring so the agent can fix deterministically.
                            let latest_log = thread_store.get(thread_id).await.ok();
                            match control_flow::gate_authoring_completion(
                                latest_log.as_ref(),
                                phase,
                            ) {
                                control_flow::AuthoringGate::Allow => {}
                                control_flow::AuthoringGate::AwaitUser { prompt } => {
                                    return Ok(vec![FlowFrame::AwaitUser { prompt }]);
                                }
                                control_flow::AuthoringGate::Block { reason } => {
                                    let ts = chrono::Utc::now().to_rfc3339();
                                    let step = react_core::session::ThreadStep::GuardBlock {
                                        phase: phase.as_str().to_string(),
                                        kind: "authoring_completion".to_string(),
                                        reason: reason.clone(),
                                        observation: react_core::session::Observation::fail(vec![
                                            reason.clone(),
                                        ]),
                                        ts,
                                        agent: "agent".to_string(),
                                    };
                                    let _ = thread_store.append_step(thread_id, step.clone()).await;
                                    let trigger_step_idx = thread_store
                                        .get(thread_id)
                                        .await
                                        .ok()
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
                                    .await?;
                                    continue;
                                }
                            }
                            // Hard gate: if validate previously failed, do not advance unless a successful mutation
                            // (and any required probes) have been recorded since that failure.
                            let latest_log = thread_store.get(thread_id).await.ok();
                            match control_flow::gate_authoring_to_validate(latest_log.as_ref()) {
                                control_flow::AuthoringGate::Allow => {}
                                control_flow::AuthoringGate::AwaitUser { prompt } => {
                                    return Ok(vec![FlowFrame::AwaitUser { prompt }]);
                                }
                                control_flow::AuthoringGate::Block { reason } => {
                                    let ts = chrono::Utc::now().to_rfc3339();
                                    let step = react_core::session::ThreadStep::GuardBlock {
                                        phase: phase.as_str().to_string(),
                                        kind: "authoring_to_validate".to_string(),
                                        reason: reason.clone(),
                                        observation: react_core::session::Observation::fail(vec![
                                            reason.clone(),
                                        ]),
                                        ts,
                                        agent: "agent".to_string(),
                                    };
                                    let _ = thread_store.append_step(thread_id, step.clone()).await;
                                    let trigger_step_idx = thread_store
                                        .get(thread_id)
                                        .await
                                        .ok()
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
                                    .await?;
                                    continue;
                                }
                            }

                            // Model authoring must actually produce at least one gold model SQL file.
                            // Without this, we can "succeed" in silver but never create any gold schema objects.
                            if !is_cleanse {
                                let actx = Self::agent_tool_ctx(thread_id, sctx);
                                if !Self::has_any_gold_model_sql(&actx).await {
                                    let reason = "No gold models were found under models/core/ or models/marts/ after ModelAuthor. Gold must be explicitly authored (marts/core SQL) before validating/publishing.";
                                    let ts = chrono::Utc::now().to_rfc3339();
                                    let step = react_core::session::ThreadStep::GuardBlock {
                                        phase: phase.as_str().to_string(),
                                        kind: "missing_gold_models".to_string(),
                                        reason: reason.to_string(),
                                        observation: react_core::session::Observation::fail(vec![
                                            reason.to_string(),
                                        ]),
                                        ts,
                                        agent: "agent".to_string(),
                                    };
                                    let _ = thread_store.append_step(thread_id, step.clone()).await;
                                    let trigger_step_idx = thread_store
                                        .get(thread_id)
                                        .await
                                        .ok()
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
                                            "kind": "missing_gold_models",
                                            "reason": reason,
                                            "expected_prefixes": ["models/core/", "models/marts/"],
                                            "trigger_step_idx": trigger_step_idx,
                                            "trigger_step": step,
                                        })),
                                    )
                                    .await?;
                                    continue;
                                }
                            }

                            // Plan-driven authoring: do NOT advance to validate until the approved plan's tasks are done.
                            if is_cleanse {
                                if let Some(p) =
                                    crate::data_engineer::plan::load_cleanse_plan(&actx).await
                                {
                                    if !crate::data_engineer::plan::cleanse_all_done(&p) {
                                        continue;
                                    }
                                }
                            } else {
                                if let Some(p) =
                                    crate::data_engineer::plan::load_model_plan(&actx).await
                                {
                                    if !crate::data_engineer::plan::model_all_done(&p) {
                                        continue;
                                    }
                                }
                            }

                            let to_phase = if is_cleanse {
                                Phase::CleanseValidate
                            } else {
                                Phase::ModelValidate
                            };
                            let reason_detail = Self::authoring_complete_reason_detail(
                                &thread_store,
                                thread_id,
                                has_proj,
                                has_models,
                            )
                            .await;
                            control_flow::append_phase_with_reason(
                                &thread_store,
                                thread_id,
                                Some("agent".to_string()),
                                Some(phase),
                                to_phase,
                                Some("authoring_complete"),
                                Some(reason_detail),
                            )
                            .await?;
                            continue;
                        }
                        Ok(RunOutcome::AwaitUser { prompt, .. }) => {
                            return Ok(vec![FlowFrame::AwaitUser { prompt }])
                        }
                        Ok(RunOutcome::AwaitApproval { prompt, .. }) => {
                            return Ok(vec![FlowFrame::AwaitApproval { prompt }])
                        }
                        Err(e) => return Err(e),
                    }
                }

                Phase::CleanseValidate | Phase::ModelValidate => {
                    let actx = Self::agent_tool_ctx(thread_id, sctx);
                    let emit_trace = |ctx: &react_core::agent::AgentCtx, line: &str| {
                        if let Some(tx) = ctx.trace_tx.as_ref() {
                            let _ = tx.send(line.to_string());
                        }
                    };

                    // Cheap structural prechecks: fail fast on malformed/duplicated schema artifacts
                    // instead of burning a full dbt_validate cycle.
                    if let Err(e) = crate::data_engineer::schema_policy::prevalidate_dbt_schema_artifacts(&actx).await {
                        let reason = format!("Pre-validation failed; fix DBT YAML artifacts before re-validating.\n\n{e}");
                        let ts = chrono::Utc::now().to_rfc3339();
                        let step = react_core::session::ThreadStep::GuardBlock {
                            phase: phase.as_str().to_string(),
                            kind: "precheck_failed".to_string(),
                            reason: reason.clone(),
                            observation: react_core::session::Observation::fail(vec![reason.clone()]),
                            ts,
                            agent: "agent".to_string(),
                        };
                        let _ = thread_store.append_step(thread_id, step).await;
                        let to_phase = if phase == Phase::CleanseValidate {
                            Phase::CleanseAuthor
                        } else {
                            Phase::ModelAuthor
                        };
                        let _ = control_flow::append_phase_with_reason(
                            &thread_store,
                            thread_id,
                            Some("agent".to_string()),
                            Some(phase),
                            to_phase,
                            Some("precheck_failed"),
                            Some(serde_json::json!({ "error": e })),
                        )
                        .await;
                        continue;
                    }

                    // Targeted pre-check (compile selected, then build selected) based on most recent patch.
                    // If it fails, we skip full validation and proceed with the standard failure handling.
                    let obs: serde_json::Value;
                    let log_now = thread_store.get(thread_id).await.ok();
                    let select_terms: Vec<String> = if let Some(l) = log_now.as_ref() {
                        control_flow::derive_targeted_select_terms(&actx, l).await
                    } else {
                        Vec::new()
                    };
                    if !select_terms.is_empty() {
                        emit_trace(&actx, "targeted compile started");
                        let args_compile = serde_json::json!({"build": false, "run": false, "select": select_terms.clone(), "targeted": true, "targeted_step": "compile"});
                        let tool_id_compile = uuid::Uuid::new_v4().to_string();
                        let _ = thread_store
                            .append_step(
                                thread_id,
                                react_core::session::ThreadStep::ToolStart {
                                    tool_id: tool_id_compile.clone(),
                                    name: "dbt_validate".to_string(),
                                    clean_name: "Validate DBT (compile)".to_string(),
                                    args: args_compile.clone(),
                                    status: "running".to_string(),
                                    payload: None,
                                    ctx: None,
                                    ts: chrono::Utc::now().to_rfc3339(),
                                    agent: "agent".to_string(),
                                },
                            )
                            .await;
                        let obs_compile = control_flow::DeterministicDbtValidateTargetedOnce::run(
                            &actx,
                            &select_terms,
                            false,
                            false,
                        )
                        .await?;
                        let obs_compile_norm =
                            react_core::session::ToolObservation::normalize(obs_compile.clone());
                        let _ = thread_store
                            .append_step(
                                thread_id,
                                react_core::session::ThreadStep::ToolEnd {
                                    tool_id: tool_id_compile,
                                    name: "dbt_validate".to_string(),
                                    clean_name: "Validate DBT (compile)".to_string(),
                                    args: args_compile,
                                    status: if obs_compile_norm.ok {
                                        "ok".to_string()
                                    } else {
                                        "failed".to_string()
                                    },
                                    payload: None,
                                    ctx: None,
                                    observation: obs_compile_norm,
                                    ts: chrono::Utc::now().to_rfc3339(),
                                    agent: "agent".to_string(),
                                },
                            )
                            .await;
                        let ok = obs_compile
                            .get("ok")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let compile_ok = obs_compile
                            .get("compile_ok")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        if !(ok && compile_ok) {
                            emit_trace(&actx, "targeted compile failed");
                            obs = obs_compile;
                        } else {
                            emit_trace(&actx, "targeted compile ok");
                            emit_trace(&actx, "targeted build started");
                            let args_build = serde_json::json!({"build": true, "run": false, "select": select_terms.clone(), "targeted": true, "targeted_step": "build"});
                            let tool_id_build = uuid::Uuid::new_v4().to_string();
                            let _ = thread_store
                                .append_step(
                                    thread_id,
                                    react_core::session::ThreadStep::ToolStart {
                                        tool_id: tool_id_build.clone(),
                                        name: "dbt_validate".to_string(),
                                        clean_name: "Validate DBT (build)".to_string(),
                                        args: args_build.clone(),
                                        status: "running".to_string(),
                                        payload: None,
                                        ctx: None,
                                        ts: chrono::Utc::now().to_rfc3339(),
                                        agent: "agent".to_string(),
                                    },
                                )
                                .await;
                            let obs_build =
                                control_flow::DeterministicDbtValidateTargetedOnce::run(
                                    &actx,
                                    &select_terms,
                                    true,
                                    false,
                                )
                                .await?;
                            let obs_build_norm =
                                react_core::session::ToolObservation::normalize(obs_build.clone());
                            let _ = thread_store
                                .append_step(
                                    thread_id,
                                    react_core::session::ThreadStep::ToolEnd {
                                        tool_id: tool_id_build,
                                        name: "dbt_validate".to_string(),
                                        clean_name: "Validate DBT (build)".to_string(),
                                        args: args_build,
                                        status: if obs_build_norm.ok {
                                            "ok".to_string()
                                        } else {
                                            "failed".to_string()
                                        },
                                        payload: None,
                                        ctx: None,
                                        observation: obs_build_norm,
                                        ts: chrono::Utc::now().to_rfc3339(),
                                        agent: "agent".to_string(),
                                    },
                                )
                                .await;
                            let ok = obs_build
                                .get("ok")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            let compile_ok = obs_build
                                .get("compile_ok")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            let run_ok = obs_build
                                .get("run_ok")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            if !(ok && compile_ok && run_ok) {
                                emit_trace(&actx, "targeted build failed");
                                obs = obs_build;
                            } else {
                                emit_trace(&actx, "targeted build ok");
                                // Deterministic full validate (NO repair loop / no mutation).
                                let args_full = serde_json::json!({"build": true});
                                let tool_id_full = uuid::Uuid::new_v4().to_string();
                                let _ = thread_store
                                    .append_step(
                                        thread_id,
                                        react_core::session::ThreadStep::ToolStart {
                                            tool_id: tool_id_full.clone(),
                                            name: "dbt_validate".to_string(),
                                            clean_name: "Validate DBT".to_string(),
                                            args: args_full.clone(),
                                            status: "running".to_string(),
                                            payload: None,
                                            ctx: None,
                                            ts: chrono::Utc::now().to_rfc3339(),
                                            agent: "agent".to_string(),
                                        },
                                    )
                                    .await;
                                obs = control_flow::DeterministicDbtValidateOnce::run(
                                    &actx, true, false, None,
                                )
                                .await?;
                                let obs_norm =
                                    react_core::session::ToolObservation::normalize(obs.clone());
                                let _ = thread_store
                                    .append_step(
                                        thread_id,
                                        react_core::session::ThreadStep::ToolEnd {
                                            tool_id: tool_id_full,
                                            name: "dbt_validate".to_string(),
                                            clean_name: "Validate DBT".to_string(),
                                            args: args_full,
                                            status: if obs_norm.ok {
                                                "ok".to_string()
                                            } else {
                                                "failed".to_string()
                                            },
                                            payload: None,
                                            ctx: None,
                                            observation: obs_norm,
                                            ts: chrono::Utc::now().to_rfc3339(),
                                            agent: "agent".to_string(),
                                        },
                                    )
                                    .await;
                            }
                        }
                    } else {
                        // Deterministic full validate (NO repair loop / no mutation).
                        let args_full = serde_json::json!({"build": true});
                        let tool_id_full = uuid::Uuid::new_v4().to_string();
                        let _ = thread_store
                            .append_step(
                                thread_id,
                                react_core::session::ThreadStep::ToolStart {
                                    tool_id: tool_id_full.clone(),
                                    name: "dbt_validate".to_string(),
                                    clean_name: "Validate DBT".to_string(),
                                    args: args_full.clone(),
                                    status: "running".to_string(),
                                    payload: None,
                                    ctx: None,
                                    ts: chrono::Utc::now().to_rfc3339(),
                                    agent: "agent".to_string(),
                                },
                            )
                            .await;
                        obs = control_flow::DeterministicDbtValidateOnce::run(
                            &actx, true, false, None,
                        )
                        .await?;
                        let obs_norm = react_core::session::ToolObservation::normalize(obs.clone());
                        let _ = thread_store
                            .append_step(
                                thread_id,
                                react_core::session::ThreadStep::ToolEnd {
                                    tool_id: tool_id_full,
                                    name: "dbt_validate".to_string(),
                                    clean_name: "Validate DBT".to_string(),
                                    args: args_full,
                                    status: if obs_norm.ok {
                                        "ok".to_string()
                                    } else {
                                        "failed".to_string()
                                    },
                                    payload: None,
                                    ctx: None,
                                    observation: obs_norm,
                                    ts: chrono::Utc::now().to_rfc3339(),
                                    agent: "agent".to_string(),
                                },
                            )
                            .await;
                    }

                    let ok = obs.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
                    let compile_ok = obs
                        .get("compile_ok")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let run_ok = obs.get("run_ok").and_then(|v| v.as_bool()).unwrap_or(false);
                    if ok && compile_ok && run_ok {
                        // Mark the active plan completed only after validate passes.
                        let latest_log = thread_store.get(thread_id).await.ok();
                        if phase == Phase::CleanseValidate {
                            if let Some(mut p) =
                                crate::data_engineer::plan::load_cleanse_plan_any(&actx).await
                            {
                                if let Some(ref l) = latest_log {
                                    crate::data_engineer::plan::update_cleanse_progress_from_log(
                                        &mut p, l,
                                    );
                                }
                                p.status = crate::data_engineer::plan::PlanStatus::Completed;
                                let _ =
                                    crate::data_engineer::plan::save_cleanse_plan(&actx, &p).await;
                            }
                        } else {
                            if let Some(mut p) =
                                crate::data_engineer::plan::load_model_plan_any(&actx).await
                            {
                                if let Some(ref l) = latest_log {
                                    crate::data_engineer::plan::update_model_progress_from_log(
                                        &mut p, l,
                                    );
                                }
                                p.status = crate::data_engineer::plan::PlanStatus::Completed;
                                let _ =
                                    crate::data_engineer::plan::save_model_plan(&actx, &p).await;
                            }
                        }

                        let trigger_step_idx = thread_store
                            .get(thread_id)
                            .await
                            .map(|l| l.steps.len().saturating_sub(1))
                            .unwrap_or(0);
                        let to_phase = if phase == Phase::CleanseValidate {
                            Phase::CleanseReview
                        } else {
                            Phase::ModelReview
                        };
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

                    // Plan-driven repair: if validation failed, reopen the failing item(s) so authoring
                    // runs a targeted fix pass instead of bouncing validate<->author with an empty batch.
                    //
                    // We infer failing models from dbt stdout lines like:
                    // `Failure in model <name> (models/.../<name>.sql)`
                    let failing_models: Vec<serde_json::Value> =
                        dbt_error::extract_failed_models_from_logs(
                            &obs.get("logs").cloned().unwrap_or(serde_json::Value::Null),
                        );
                    let brief = dbt_error::compact_brief(&errs, 6, 1200);
                    if !failing_models.is_empty() {
                        if phase == Phase::CleanseValidate {
                            if let Some(mut p) =
                                crate::data_engineer::plan::load_cleanse_plan(&actx).await
                            {
                                let mut reopened: Vec<String> = Vec::new();
                                let mut want_model_names: Vec<String> = Vec::new();
                                let mut want_file_stems: Vec<String> = Vec::new();
                                for fm in failing_models.iter() {
                                    if let Some(n) = fm.get("name").and_then(|v| v.as_str()) {
                                        want_model_names.push(n.to_string());
                                    }
                                    if let Some(f) = fm.get("file").and_then(|v| v.as_str()) {
                                        if let Some(stem) = std::path::Path::new(f)
                                            .file_stem()
                                            .map(|s| s.to_string_lossy().to_string())
                                        {
                                            want_file_stems.push(stem);
                                        }
                                    }
                                }
                                want_model_names.sort();
                                want_model_names.dedup();
                                want_file_stems.sort();
                                want_file_stems.dedup();

                                for t in p.tasks.iter_mut() {
                                    let mut expected_name: Option<String> = None;
                                    if let Some(ref rel) = t.expected_model_path {
                                        expected_name = std::path::Path::new(rel)
                                            .file_stem()
                                            .map(|s| s.to_string_lossy().to_string());
                                    }
                                    if expected_name.is_none() {
                                        let parts: Vec<&str> = t.dataset_id.split('.').collect();
                                        if parts.len() == 3 {
                                            expected_name = Some(crate::data_engineer::naming::canonical_staging_model_name(
                                                parts[1],
                                                parts[2],
                                            ));
                                        }
                                    }
                                    let Some(expected) = expected_name else {
                                        continue;
                                    };
                                    if want_model_names.contains(&expected)
                                        || want_file_stems.contains(&expected)
                                    {
                                        // Mark the task as needing attention again. Task status should be derived
                                        // from checklist items; we only adjust the coarse status here as a hint and
                                        // store detailed failure context into plan.project_snapshot.
                                        t.status = crate::data_engineer::plan::TaskStatus::InProgress;
                                        reopened.push(t.dataset_id.clone());
                                    }
                                }
                                reopened.sort();
                                reopened.dedup();
                                if !reopened.is_empty() {
                                    let mut plan_note = format!(
                                        "validate_fail reopened {} staging task(s): {}",
                                        reopened.len(),
                                        reopened.join(", ")
                                    );
                                    if !want_model_names.is_empty() {
                                        plan_note.push_str(&format!(
                                            "\nFailing model(s): {}",
                                            want_model_names.join(", ")
                                        ));
                                    }
                                    if p.project_snapshot.is_null() {
                                        p.project_snapshot = serde_json::json!({});
                                    }
                                    if let Some(obj) = p.project_snapshot.as_object_mut() {
                                        let arr = obj
                                            .entry("validate_fail_notes")
                                            .or_insert_with(|| serde_json::Value::Array(vec![]));
                                        if let Some(a) = arr.as_array_mut() {
                                            a.push(serde_json::Value::String(plan_note));
                                            // Keep bounded.
                                            while a.len() > 10 {
                                                a.remove(0);
                                            }
                                        }
                                    }
                                    let _ =
                                        crate::data_engineer::plan::save_cleanse_plan(&actx, &p)
                                            .await;
                                }
                            }
                        } else {
                            // Model (gold) plan: reopen failing model tasks by model name/file stem match.
                            if let Some(mut p) =
                                crate::data_engineer::plan::load_model_plan(&actx).await
                            {
                                let mut want_names: HashSet<String> = HashSet::new();
                                for fm in failing_models.iter() {
                                    if let Some(n) = fm.get("name").and_then(|v| v.as_str()) {
                                        want_names.insert(n.to_string());
                                    }
                                    if let Some(f) = fm.get("file").and_then(|v| v.as_str()) {
                                        if let Some(stem) = std::path::Path::new(f)
                                            .file_stem()
                                            .map(|s| s.to_string_lossy().to_string())
                                        {
                                            want_names.insert(stem);
                                        }
                                    }
                                }
                                let mut reopened: Vec<String> = Vec::new();
                                for t in p.tasks.iter_mut() {
                                    let mut expected: Option<String> = None;
                                    if !t.name.trim().is_empty() {
                                        expected = Some(t.name.trim().to_string());
                                    }
                                    if let Some(ref rel) = t.expected_model_path {
                                        if let Some(stem) = std::path::Path::new(rel)
                                            .file_stem()
                                            .map(|s| s.to_string_lossy().to_string())
                                        {
                                            expected = Some(stem);
                                        }
                                    }
                                    let Some(exp) = expected else { continue };
                                    if want_names.contains(&exp) {
                                        // Mark the task as needing attention again. Task status should be derived
                                        // from checklist items; we only adjust the coarse status here as a hint.
                                        t.status = crate::data_engineer::plan::TaskStatus::InProgress;
                                        reopened.push(t.name.clone());
                                    }
                                }
                                reopened.sort();
                                reopened.dedup();
                                if !reopened.is_empty() {
                                    let _ = crate::data_engineer::plan::save_model_plan(&actx, &p)
                                        .await;
                                }
                            }
                        }
                    }

                    // Validation failed -> go back to corresponding author phase.
                    let trigger_step_idx = thread_store
                        .get(thread_id)
                        .await
                        .map(|l| l.steps.len().saturating_sub(1))
                        .unwrap_or(0);
                    let to_phase = if phase == Phase::CleanseValidate {
                        Phase::CleanseAuthor
                    } else {
                        Phase::ModelAuthor
                    };
                    // Attach authoritative schema facts for the next authoring turn. This ensures the LLM
                    // never needs to guess relation columns after a deterministic validate failure.
                    let dialect = obs
                        .get("dialect")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Unknown SQL dialect")
                        .to_string();
                    let facts_bundle = crate::data_engineer::facts::build_validate_fail_facts(
                        &actx,
                        dialect,
                        &obs,
                        crate::data_engineer::facts::FactsScope::ValidateFail,
                        crate::data_engineer::facts::FactsLimits::for_scope(
                            crate::data_engineer::facts::FactsScope::ValidateFail,
                        ),
                    )
                    .await;
                    // Best-effort persist into the active plan snapshot for reuse in subsequent authoring turns.
                    // Keep bounded to avoid unbounded plan growth.
                    if phase == Phase::CleanseValidate {
                        if let Some(mut p) =
                            crate::data_engineer::plan::load_cleanse_plan(&actx).await
                        {
                            if p.project_snapshot.is_null() {
                                p.project_snapshot = serde_json::json!({});
                            }
                            if let Some(obj) = p.project_snapshot.as_object_mut() {
                                let arr = obj
                                    .entry("validate_fail_facts")
                                    .or_insert_with(|| serde_json::Value::Array(vec![]));
                                if let Some(a) = arr.as_array_mut() {
                                    a.push(
                                        serde_json::to_value(&facts_bundle)
                                            .unwrap_or(serde_json::Value::Null),
                                    );
                                    while a.len() > 5 {
                                        a.remove(0);
                                    }
                                }
                            }
                            let _ = crate::data_engineer::plan::save_cleanse_plan(&actx, &p).await;
                        }
                    } else {
                        if let Some(mut p) =
                            crate::data_engineer::plan::load_model_plan(&actx).await
                        {
                            if p.project_snapshot.is_null() {
                                p.project_snapshot = serde_json::json!({});
                            }
                            if let Some(obj) = p.project_snapshot.as_object_mut() {
                                let arr = obj
                                    .entry("validate_fail_facts")
                                    .or_insert_with(|| serde_json::Value::Array(vec![]));
                                if let Some(a) = arr.as_array_mut() {
                                    a.push(
                                        serde_json::to_value(&facts_bundle)
                                            .unwrap_or(serde_json::Value::Null),
                                    );
                                    while a.len() > 5 {
                                        a.remove(0);
                                    }
                                }
                            }
                            let _ = crate::data_engineer::plan::save_model_plan(&actx, &p).await;
                        }
                    }
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
                            "facts_bundle": facts_bundle,
                        })),
                    )
                    .await;
                    continue;
                }

                Phase::CleanseReview | Phase::ModelReview | Phase::PostPublishReview => {
                    let review_q =
                        Self::build_review_question_with_context(question, phase, log.as_ref());
                    let frames =
                        review_batched::run_batched_review(thread_id, &review_q, phase, sctx)
                            .await?;
                    let first = frames.into_iter().next().unwrap_or(FlowFrame::Final {
                        kind: "generic".to_string(),
                        payload: serde_json::json!({ "text": "" }),
                        display: None,
                    });
                    let answer = match first {
                        FlowFrame::Final {
                            payload, display, ..
                        } => display
                            .or_else(|| {
                                payload
                                    .get("text")
                                    .and_then(|x| x.as_str())
                                    .map(|s| s.to_string())
                            })
                            .unwrap_or_default(),
                        other => return Ok(vec![other]),
                    };

                    // Best-effort: capture the review final step as the trigger for phase transitions.
                    let (trigger_step_idx, trigger_step) = thread_store
                        .get(thread_id)
                        .await
                        .ok()
                        .and_then(|l| {
                            let idx = l.steps.len().saturating_sub(1);
                            l.steps.last().cloned().map(|s| (idx, s))
                        })
                        .unwrap_or((
                            0,
                            react_core::session::ThreadStep::GuardBlock {
                                phase: "unknown".to_string(),
                                kind: "missing_thread_step".to_string(),
                                reason: "missing thread step".to_string(),
                                observation: react_core::session::Observation::fail(vec![
                                    "missing thread step".to_string(),
                                ]),
                                ts: chrono::Utc::now().to_rfc3339(),
                                agent: "agent".to_string(),
                            },
                        ));

                    let meta = Self::parse_review_meta(&answer).unwrap_or(ReviewMeta {
                        actionable: false,
                        dataset_ids: vec![],
                        tier: "unknown".to_string(),
                    });
                    // Extract review_ref (if present) from the trigger step so we can persist a stable pointer
                    // without embedding the full review text in subsequent phase transitions.
                    let review_ref = match &trigger_step {
                        react_core::session::ThreadStep::Phase { reason_detail, .. } => reason_detail
                            .as_ref()
                            .and_then(|v| v.get("review_ref"))
                            .cloned(),
                        _ => None,
                    };
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
                            Phase::CleanseReview => Phase::ModelPlan,
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
                                "review_ref": review_ref,
                                "trigger_step_idx": trigger_step_idx,
                            })),
                        )
                        .await;
                        continue;
                    }

                    // Actionable review -> route back to appropriate authoring phase.
                    let tier = meta.tier.trim().to_lowercase();
                    let back = if tier == "silver" {
                        Phase::CleansePlan
                    } else {
                        Phase::ModelPlan
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
                            "review_ref": review_ref,
                            "trigger_step_idx": trigger_step_idx,
                        })),
                    )
                    .await;
                    continue;
                }

                Phase::PublishAwaitApproval => {
                    // If the most recent persisted user action is "reject", stop and ask for guidance.
                    if let Some(ref l) = log {
                        if let Some(last_user) = l
                            .steps
                            .iter()
                            .rev()
                            .find(|s| matches!(s, react_core::session::ThreadStep::User { .. }))
                        {
                            let decision = match last_user {
                                react_core::session::ThreadStep::User { text, .. } => {
                                    Self::parse_user_decision(text)
                                }
                                _ => None,
                            };
                            if decision == Some(UserDecision::Reject) {
                                return Ok(vec![FlowFrame::AwaitUser {
                                    prompt: "Publish was rejected. Provide guidance (e.g. restrict dataset_ids, change materializations, or adjust models) and then re-run agent.".to_string(),
                                }]);
                            }
                            if decision == Some(UserDecision::Approve) {
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
                                .await?;
                                continue;
                            }
                        }
                    }

                    let actx = Self::agent_tool_ctx(thread_id, sctx);
                    let tool = tools::publish_dbt_to_provider::PublishDbtToProviderTool {
                        datasets: sctx.datasets.clone(),
                        catalog: sctx.catalog.clone(),
                    };
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
                        let prompt = obs
                            .get("prompt")
                            .and_then(|v| v.as_str())
                            .unwrap_or("Approve to publish?")
                            .to_string();
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
                        if let Some(last_user) = l
                            .steps
                            .iter()
                            .rev()
                            .find(|s| matches!(s, react_core::session::ThreadStep::User { .. }))
                        {
                            let decision = match last_user {
                                react_core::session::ThreadStep::User { text, .. } => {
                                    Self::parse_user_decision(text)
                                }
                                _ => None,
                            };
                            if decision != Some(UserDecision::Approve) {
                                return Ok(vec![FlowFrame::AwaitApproval {
                                    prompt: "Publish requires explicit approval. Click Approve to continue.".to_string(),
                                }]);
                            }
                        }
                    }
                    let actx = Self::agent_tool_ctx(thread_id, sctx);
                    let tool = tools::publish_dbt_to_provider::PublishDbtToProviderTool {
                        datasets: sctx.datasets.clone(),
                        catalog: sctx.catalog.clone(),
                    };
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
                    out_frames.push(FlowFrame::Final {
                        kind: "generic".to_string(),
                        payload: serde_json::json!({ "text": answer.clone() }),
                        display: Some(answer),
                    });
                    return Ok(out_frames);
                }
            }
        }

        Ok(vec![FlowFrame::AwaitUser {
            prompt: format!(
                "Agent reached the phase-step budget without completing.\n\nBudget:\n- max_steps_per_progress={max_phase_steps}\n- total_steps={total_steps}\n\nThis usually indicates a loop (repeatedly re-entering the same phase without durable progress). Review thread history and retry with more specific instructions, or increase AGENT_MAX_PHASE_STEPS."
            ),
        }])
    }

    async fn run_authoring(
        kind: AuthoringKind,
        thread_id: &str,
        question: &str,
        sctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String> {
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
        let bundle = pf
            .run(thread_id, question, agent_name, sctx)
            .await
            .discovery;

        let registry = Self::build_tools(agent_name, sctx)?;
        let thread_store = ThreadStore::new(
            sctx.storage.clone(),
            sctx.scope.clone(),
            sctx.keyspace.clone(),
        );

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
            warehouse: sctx.warehouse.clone(),
            dbt: sctx.dbt.clone(),
            vector: sctx.vector.clone(),
            thread_store: Some(thread_store.clone()),
            exec_ctx: None,
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

        let llm_options = match kind {
            AuthoringKind::Cleanse => Some(LlmCallOptions {
                temperature: Some(0.05),
                top_p: Some(1.0),
                max_output_tokens: Some(1800),
            }),
            AuthoringKind::Model => Some(LlmCallOptions {
                temperature: Some(0.12),
                top_p: Some(1.0),
                max_output_tokens: Some(2200),
            }),
        };

        for attempt in 0..10 {
            match Agent::run_until_block(&registry, &actx, &sys, &tools_card, &prompt, llm_options).await {
                Ok(RunOutcome::Final {
                    thread_id: _tid,
                    result,
                }) => {
                    last_final = Some(result.clone());

                    // Post-run validate (includes build) so runtime failures feed back into auto-remediation.
                    let validate_tool = tools::dbt_validate::DbtValidateTool {
                        datasets: sctx.datasets.clone(),
                        catalog: sctx.catalog.clone(),
                    };
                    let args = json!({
                        "project_name": format!("{}_project", sctx.scope.project_id.replace('/', "_")),
                        "build": true
                    });
                    let obs = validate_tool
                        .call(args, &actx)
                        .await
                        .unwrap_or_else(|e| json!({"ok": false, "error": e}));
                    let ok = obs.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
                    let compile_ok = obs
                        .get("compile_ok")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let run_ok = obs.get("run_ok").and_then(|v| v.as_bool()).unwrap_or(false);
                    if ok && compile_ok && run_ok {
                        return Ok(vec![FlowFrame::Final {
                            kind: result.kind,
                            payload: result.payload,
                            display: result.display,
                        }]);
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
                            let name = rf
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown_test");
                            let n = rf
                                .get("failures")
                                .and_then(|v| v.as_u64())
                                .map(|x| x.to_string())
                                .unwrap_or("?".to_string());
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
                        let manifest_lines =
                            Self::manifest_targeting_lines(&actx, &runtime_failures).await;
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
                            "Auto-remediation attempt {}: dbt_validate/build failed.\n\nError summary:\n{}\n\nAutomatically fix the DBT project:\n- Prefer calling `staging_model` to update staging/silver models (nested fields, cleansing, naming).\n- Use dbt_files or artifacts to inspect/edit existing files.\n- Re-run dbt_validate with build=true.\nRepeat until compile_ok=true AND run_ok=true.",
                            attempt + 1,
                            brief
                        );
                    }
                    continue;
                }
                Ok(RunOutcome::AwaitUser {
                    thread_id: _tid,
                    prompt: p,
                }) => {
                    return Ok(vec![FlowFrame::AwaitUser { prompt: p }]);
                }
                Ok(RunOutcome::AwaitApproval {
                    thread_id: _tid,
                    prompt: p,
                }) => {
                    return Ok(vec![FlowFrame::AwaitApproval { prompt: p }]);
                }
                Err(e) => return Err(e),
            }
        }

        if let Some(r) = last_final {
            return Ok(vec![FlowFrame::Final {
                kind: r.kind,
                payload: r.payload,
                display: r.display,
            }]);
        }
        Err(format!("{}: no outcome", agent_name))
    }

    async fn run_cleanse(
        thread_id: &str,
        question: &str,
        sctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String> {
        Self::run_authoring(AuthoringKind::Cleanse, thread_id, question, sctx).await
    }

    async fn run_model(
        thread_id: &str,
        question: &str,
        sctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String> {
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
            Phase::CleansePlan.as_str(),
            Phase::CleanseAuthor.as_str(),
            Phase::CleanseValidate.as_str(),
            Phase::CleanseReview.as_str(),
            Phase::ModelPlan.as_str(),
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

    #[derive(Clone)]
    struct MockWarehouseOk {
        ok_fqns: std::collections::HashSet<String>,
    }

    #[async_trait]
    impl react_core::providers::QueryProvider for MockWarehouseOk {
        async fn query(&self, _sql: &str) -> Result<react_core::providers::QueryResult, String> {
            Ok(react_core::providers::QueryResult {
                header: vec![],
                rows: vec![],
                meta: None,
            })
        }

        async fn schema(&self, dataset_fqn: &str) -> Result<Vec<(String, String)>, String> {
            if self.ok_fqns.contains(dataset_fqn) {
                Ok(vec![("x".to_string(), "string".to_string())])
            } else {
                Err("not found".to_string())
            }
        }

        async fn sample(
            &self,
            _dataset_fqn: &str,
            _limit: usize,
        ) -> Result<Vec<Vec<String>>, String> {
            Ok(vec![])
        }
    }

    #[async_trait]
    impl react_core::providers::DatasetCatalogProvider for MockWarehouseOk {
        async fn list_datasets(&self) -> Result<Vec<react_core::providers::DatasetId>, String> {
            Ok(vec![])
        }

        async fn get_dataset_schema(
            &self,
            dataset: &react_core::providers::DatasetId,
        ) -> Result<Vec<(String, String)>, String> {
            let fqn = dataset.fqn();
            react_core::providers::QueryProvider::schema(self, fqn.as_str()).await
        }

        async fn get_dataset_stats(
            &self,
            _dataset: &react_core::providers::DatasetId,
            _max_fields: usize,
        ) -> Result<
            (
                react_core::discover::stats::DatasetFieldStats,
                react_core::providers::catalog::types::DatasetStats,
            ),
            String,
        > {
            Err("not used".to_string())
        }
    }

    impl react_core::providers::WarehouseNaming for MockWarehouseOk {
        fn kind(&self) -> &'static str {
            "mock"
        }

        fn parse_dataset_fqn(
            &self,
            dataset_fqn: &str,
        ) -> Result<react_core::providers::DatasetId, String> {
            let parts: Vec<&str> = dataset_fqn.split('.').collect();
            if parts.len() != 3 {
                return Err("expected <catalog>.<schema>.<table>".to_string());
            }
            Ok(react_core::providers::DatasetId {
                catalog: parts[0].to_string(),
                database: parts[1].to_string(),
                table: parts[2].to_string(),
            })
        }

        fn quote_ident(&self, ident: &str) -> String {
            format!("\"{}\"", ident.replace('"', "\"\""))
        }
    }

    struct MockQuery;

    #[async_trait]
    impl react_core::providers::QueryProvider for MockQuery {
        async fn query(&self, _sql: &str) -> Result<react_core::providers::QueryResult, String> {
            Ok(react_core::providers::QueryResult {
                header: vec![],
                rows: vec![],
                meta: None,
            })
        }
        async fn schema(&self, _dataset_fqn: &str) -> Result<Vec<(String, String)>, String> {
            Ok(vec![])
        }
        async fn sample(
            &self,
            _dataset_fqn: &str,
            _limit: usize,
        ) -> Result<Vec<Vec<String>>, String> {
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn review_registry_is_read_only() {
        let mut sctx = SuiteCtx::default();
        sctx.query = Some(Arc::new(MockQuery));

        let reg = DataEngineerSuite::build_tools("review", &sctx)
            .expect("build_tools(review) should succeed");
        let actx = DataEngineerSuite::agent_tool_ctx("t", &sctx);

        // Not allowed in review
        assert!(reg
            .call("approve_and_save_artifact", serde_json::json!({}), &actx)
            .await
            .is_err());
        assert!(reg
            .call(
                "approve_and_save_artifact_batch",
                serde_json::json!({}),
                &actx
            )
            .await
            .is_err());
        assert!(reg
            .call("dbt_validate", serde_json::json!({}), &actx)
            .await
            .is_err());
        assert!(reg
            .call("publish_dbt_to_provider", serde_json::json!({}), &actx)
            .await
            .is_err());
        assert!(reg
            .call("staging_model", serde_json::json!({}), &actx)
            .await
            .is_err());
        assert!(reg
            .call("catalog_note", serde_json::json!({}), &actx)
            .await
            .is_err());
        assert!(reg
            .call("ask_user", serde_json::json!({}), &actx)
            .await
            .is_err());
        assert!(reg
            .call("ask_approval", serde_json::json!({}), &actx)
            .await
            .is_err());

        // Also exclude arbitrary SQL execution in review mode.
        assert!(reg
            .call("run_sql", serde_json::json!({"sql":"SELECT 1"}), &actx)
            .await
            .is_err());

        // Allowed in review
        let obs = reg
            .call(
                "artifacts",
                serde_json::json!({"op":"list","limit":5}),
                &actx,
            )
            .await
            .expect("artifacts should be available");
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
            patched_since_fail: false,
            mutation_failures_since_validate: 0,
            probe_required: false,
            probe_satisfied: false,
        };
        let (reg, card) = DataEngineerSuite::build_tools_for_phase(
            crate::data_engineer::control_flow::Phase::ModelAuthor,
            &guard,
            true,
            &sctx,
            None,
        )
        .expect("build_tools_for_phase should succeed");

        // Tool card should advertise patch primitives (not apply_next_* tools).
        assert!(card.contains("replace_file"));
        assert!(!card.contains("apply_next_model_batch"));

        // run_sql should not be available in hard mutation-only mode
        assert!(reg
            .call("run_sql", serde_json::json!({"sql":"SELECT 1"}), &actx)
            .await
            .is_ok());

        // dbt_files get should be blocked (put-only wrapper)
        assert!(reg
            .call(
                "dbt_files",
                serde_json::json!({"op":"get","path":"dbt_project.yml"}),
                &actx
            )
            .await
            .is_err());

        // apply_next_* tools should not be available in hard mutation-only mode
        let err = reg
            .call("apply_next_model_batch", serde_json::json!({}), &actx)
            .await
            .unwrap_err();
        assert!(err.contains("unknown tool"));
    }

    #[tokio::test]
    async fn hard_mutation_mode_does_not_expose_apply_next_cleanse_batch_even_if_allowed_batch_present(
    ) {
        let mut sctx = SuiteCtx::default();
        sctx.query = Some(Arc::new(MockQuery));
        let actx = DataEngineerSuite::agent_tool_ctx("t", &sctx);

        let guard = crate::data_engineer::control_flow::DerivedGuardState {
            last_validate_failed: true,
            mutated_since_fail: false,
            patched_since_fail: false,
            mutation_failures_since_validate: 0,
            probe_required: false,
            probe_satisfied: false,
        };

        let (reg, card) = DataEngineerSuite::build_tools_for_phase(
            crate::data_engineer::control_flow::Phase::CleanseAuthor,
            &guard,
            true,
            &sctx,
            Some(super::AllowedBatch::CleanseDatasetIds(vec![
                "AwsDataCatalog.db.t1".to_string(),
            ])),
        )
        .expect("build_tools_for_phase should succeed");

        assert!(card.contains("replace_file"));
        assert!(!card.contains("apply_next_cleanse_batch"));

        let err = reg
            .call("apply_next_cleanse_batch", serde_json::json!({}), &actx)
            .await
            .unwrap_err();
        assert!(err.contains("unknown tool"));
    }

    #[tokio::test]
    async fn plan_batched_staging_model_is_not_exposed_to_agent() {
        let mut sctx = SuiteCtx::default();
        sctx.query = Some(Arc::new(MockQuery));
        let actx = DataEngineerSuite::agent_tool_ctx("t", &sctx);

        let guard = crate::data_engineer::control_flow::DerivedGuardState::default();
        let (reg, _card) = DataEngineerSuite::build_tools_for_phase(
            crate::data_engineer::control_flow::Phase::CleanseAuthor,
            &guard,
            true,
            &sctx,
            Some(super::AllowedBatch::CleanseDatasetIds(vec![
                "AwsDataCatalog.db.t1".to_string(),
            ])),
        )
        .expect("build_tools_for_phase should succeed");

        let err = reg
            .call(
                "staging_model",
                serde_json::json!({"dataset_ids":["AwsDataCatalog.db.t2"]}),
                &actx,
            )
            .await
            .unwrap_err();
        assert!(err.contains("unknown tool"));

        // Deterministic executor tool should exist (even if it fails due to missing plan in this test ctx).
        let err2 = reg
            .call("apply_next_cleanse_batch", serde_json::json!({}), &actx)
            .await
            .unwrap_err();
        assert!(err2.contains("no active cleanse plan"));
    }

    #[tokio::test]
    async fn plan_batched_gold_model_is_not_exposed_to_agent() {
        let mut sctx = SuiteCtx::default();
        sctx.query = Some(Arc::new(MockQuery));
        let actx = DataEngineerSuite::agent_tool_ctx("t", &sctx);

        let guard = crate::data_engineer::control_flow::DerivedGuardState::default();
        let (reg, _card) = DataEngineerSuite::build_tools_for_phase(
            crate::data_engineer::control_flow::Phase::ModelAuthor,
            &guard,
            true,
            &sctx,
            Some(super::AllowedBatch::ModelItemNames(vec![
                "fct_orders".to_string()
            ])),
        )
        .expect("build_tools_for_phase should succeed");

        let err = reg
            .call(
                "gold_model",
                serde_json::json!({"items":[{"name":"dim_users","inputs":["stg_x"]}]}),
                &actx,
            )
            .await
            .unwrap_err();
        assert!(err.contains("unknown tool"));

        let err2 = reg
            .call("apply_next_model_batch", serde_json::json!({}), &actx)
            .await
            .unwrap_err();
        assert!(err2.contains("no active model plan"));
    }

    #[tokio::test]
    async fn plan_phase_auto_approves_when_entered_from_review_actionable_true_cleanse() {
        let thread_id = "t_auto_cleanse";
        let mut sctx = SuiteCtx::default();

        let ds = "AwsDataCatalog.test_raw.raw_orders".to_string();
        let mut ok = std::collections::HashSet::new();
        ok.insert(ds.clone());
        sctx.warehouse = Arc::new(MockWarehouseOk { ok_fqns: ok });

        let thread_store = ThreadStore::new(
            sctx.storage.clone(),
            sctx.scope.clone(),
            sctx.keyspace.clone(),
        );
        let actx = DataEngineerSuite::plan_agent_ctx(thread_id, &sctx);

        let plan_key = crate::data_engineer::plan::new_cleanse_plan_key(&actx);
        let plan = crate::data_engineer::plan::CleansePlan {
            plan_key: plan_key.clone(),
            status: crate::data_engineer::plan::PlanStatus::Draft,
            project_snapshot: serde_json::Value::Null,
            tasks: vec![crate::data_engineer::plan::CleanseTask {
                dataset_id: ds.clone(),
                expected_model_path: Some("models/staging/stg_test_raw_raw_orders.sql".to_string()),
                invariants: vec![],
                status: crate::data_engineer::plan::TaskStatus::Pending,
                checklist: vec![],
            }],
            batches: vec![vec![ds.clone()]],
            work_groups: vec![],
            mutations: vec![],
            progress: crate::data_engineer::plan::PlanProgress::default(),
        };
        crate::data_engineer::plan::save_cleanse_plan(&actx, &plan)
            .await
            .expect("save");

        let log = react_core::session::ThreadLog {
            steps: vec![react_core::session::ThreadStep::Phase {
                phase: control_flow::Phase::CleansePlan.as_str().to_string(),
                from_phase: Some(control_flow::Phase::CleanseReview.as_str().to_string()),
                reason_code: Some("review_actionable_true".to_string()),
                reason_detail: Some(serde_json::json!({"answer":"META:{\"actionable\":true}\n\nFix contracts."})),
                observation: react_core::session::Observation::ok(),
                ts: "t".to_string(),
                agent: "agent".to_string(),
            }],
            ..Default::default()
        };
        assert!(DataEngineerSuite::actionable_review_entry_step_idx(
            Some(&log),
            control_flow::Phase::CleansePlan
        )
        .is_some());

        let advanced = DataEngineerSuite::approve_cleanse_plan_draft_and_advance(
            &thread_store,
            thread_id,
            control_flow::Phase::CleansePlan,
            &actx,
            123,
            "plan_auto_approved",
            serde_json::json!({ "plan_key": plan_key, "entry_reason_code": "review_actionable_true" }),
        )
        .await
        .expect("approve");
        assert!(advanced);

        let loaded = crate::data_engineer::plan::load_cleanse_plan(&actx)
            .await
            .expect("plan");
        assert_eq!(loaded.status, crate::data_engineer::plan::PlanStatus::Approved);
        assert_eq!(loaded.progress.last_applied_step_idx, 123);

        let log2 = thread_store.get(thread_id).await.expect("thread log");
        let last_phase = log2.steps.iter().rev().find_map(|s| match s {
            react_core::session::ThreadStep::Phase { phase, reason_code, .. } => {
                Some((phase.clone(), reason_code.clone()))
            }
            _ => None,
        });
        let (p, rc) = last_phase.expect("phase");
        assert_eq!(p, control_flow::Phase::CleanseAuthor.as_str());
        assert_eq!(rc.as_deref(), Some("plan_auto_approved"));
    }

    #[tokio::test]
    async fn plan_phase_auto_approves_when_entered_from_review_actionable_true_model() {
        let thread_id = "t_auto_model";
        let sctx = SuiteCtx::default();

        // For model plan approval grounding, we need at least one staging model present under models/staging/.
        let base = sctx.keyspace.dbt_prefix(&sctx.scope).trim_end_matches('/').to_string();
        let stg_rel = "models/staging/stg_test_raw_raw_orders.sql";
        let stg_key = format!("{}/{}", base, stg_rel);
        sctx.storage
            .put_bytes(&stg_key, b"select 1", "text/sql")
            .await
            .expect("seed staging");

        let thread_store = ThreadStore::new(
            sctx.storage.clone(),
            sctx.scope.clone(),
            sctx.keyspace.clone(),
        );
        let actx = DataEngineerSuite::plan_agent_ctx(thread_id, &sctx);

        let plan_key = crate::data_engineer::plan::new_model_plan_key(&actx);
        let plan = crate::data_engineer::plan::ModelPlan {
            plan_key: plan_key.clone(),
            status: crate::data_engineer::plan::PlanStatus::Draft,
            project_snapshot: serde_json::Value::Null,
            tasks: vec![crate::data_engineer::plan::ModelTask {
                name: "fct_orders".to_string(),
                folder: "marts".to_string(),
                goal: "Orders fact".to_string(),
                inputs: vec!["stg_test_raw_raw_orders".to_string()],
                expected_model_path: Some("models/marts/fct_orders.sql".to_string()),
                invariants: vec![],
                status: crate::data_engineer::plan::TaskStatus::Pending,
                checklist: vec![],
            }],
            batches: vec![vec!["fct_orders".to_string()]],
            work_groups: vec![],
            mutations: vec![],
            progress: crate::data_engineer::plan::PlanProgress::default(),
        };
        crate::data_engineer::plan::save_model_plan(&actx, &plan)
            .await
            .expect("save");

        let log = react_core::session::ThreadLog {
            steps: vec![react_core::session::ThreadStep::Phase {
                phase: control_flow::Phase::ModelPlan.as_str().to_string(),
                from_phase: Some(control_flow::Phase::ModelReview.as_str().to_string()),
                reason_code: Some("review_actionable_true".to_string()),
                reason_detail: Some(serde_json::json!({"answer":"META:{\"actionable\":true}\n\nFix docs."})),
                observation: react_core::session::Observation::ok(),
                ts: "t".to_string(),
                agent: "agent".to_string(),
            }],
            ..Default::default()
        };
        assert!(DataEngineerSuite::actionable_review_entry_step_idx(
            Some(&log),
            control_flow::Phase::ModelPlan
        )
        .is_some());

        let advanced = DataEngineerSuite::approve_model_plan_draft_and_advance(
            &thread_store,
            thread_id,
            control_flow::Phase::ModelPlan,
            &actx,
            77,
            "plan_auto_approved",
            serde_json::json!({ "plan_key": plan_key, "entry_reason_code": "review_actionable_true" }),
        )
        .await
        .expect("approve");
        assert!(advanced);

        let loaded = crate::data_engineer::plan::load_model_plan(&actx)
            .await
            .expect("plan");
        assert_eq!(loaded.status, crate::data_engineer::plan::PlanStatus::Approved);
        assert_eq!(loaded.progress.last_applied_step_idx, 77);

        let log2 = thread_store.get(thread_id).await.expect("thread log");
        let last_phase = log2.steps.iter().rev().find_map(|s| match s {
            react_core::session::ThreadStep::Phase { phase, reason_code, .. } => {
                Some((phase.clone(), reason_code.clone()))
            }
            _ => None,
        });
        let (p, rc) = last_phase.expect("phase");
        assert_eq!(p, control_flow::Phase::ModelAuthor.as_str());
        assert_eq!(rc.as_deref(), Some("plan_auto_approved"));
    }

    #[test]
    fn allow_ask_approval_is_monotonic_within_phase() {
        use crate::data_engineer::control_flow::Phase;
        use react_core::session::ThreadLog;

        let log = ThreadLog {
            steps: vec![
                react_core::session::ThreadStep::Phase {
                    phase: "cleanse_author".to_string(),
                    from_phase: None,
                    reason_code: None,
                    reason_detail: None,
                    observation: react_core::session::Observation::ok(),
                    ts: "t".to_string(),
                    agent: "agent".to_string(),
                },
                react_core::session::ThreadStep::AskApproval {
                    prompt: "p".to_string(),
                    observation: react_core::session::Observation::ok(),
                    ts: "t".to_string(),
                    agent: "agent".to_string(),
                },
                react_core::session::ThreadStep::User {
                    text: "approve".to_string(),
                    observation: react_core::session::Observation::ok(),
                    ts: "t".to_string(),
                    agent: "agent".to_string(),
                },
                // Later user chatter must not re-enable ask_approval.
                react_core::session::ThreadStep::User {
                    text: "continue".to_string(),
                    observation: react_core::session::Observation::ok(),
                    ts: "t".to_string(),
                    agent: "agent".to_string(),
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            DataEngineerSuite::allow_ask_approval_in_phase(Some(&log), Phase::CleanseAuthor),
            false
        );
    }

    #[test]
    fn parse_user_decision_accepts_loose_synonyms() {
        use super::DataEngineerSuite;
        use super::UserDecision;

        for s in [
            "approve", "Approved", "approve!", "yes", "Y", "ok", "OK", "okay", "continue",
        ] {
            assert_eq!(
                DataEngineerSuite::parse_user_decision(s),
                Some(UserDecision::Approve),
                "expected approve for input={}",
                s
            );
        }
        for s in ["reject", "Rejected", "reject.", "no", "N"] {
            assert_eq!(
                DataEngineerSuite::parse_user_decision(s),
                Some(UserDecision::Reject),
                "expected reject for input={}",
                s
            );
        }
        for s in ["", "   ", "maybe", "later", "continue?maybe"] {
            assert_eq!(
                DataEngineerSuite::parse_user_decision(s),
                None,
                "expected none for input={}",
                s
            );
        }
    }

    #[test]
    fn review_question_includes_prior_review_and_mutation_diff_when_available() {
        use crate::data_engineer::control_flow::Phase;
        use react_core::session::{ThreadLog, ThreadStep};

        let prior_review_answer = "META:{\"actionable\":true,\"dataset_ids\":[\"x\"],\"tier\":\"silver\"}\n\nPlease add tests.";
        let prior_review_transition = ThreadStep::Phase {
            phase: "cleanse_author".to_string(),
            from_phase: Some("cleanse_review".to_string()),
            reason_code: Some("review_actionable_true".to_string()),
            reason_detail: Some(serde_json::json!({
                "review_phase":"cleanse_review",
                "meta": {"actionable": true, "dataset_ids": ["x"], "tier":"silver"},
                "answer": prior_review_answer
            })),
            observation: react_core::session::Observation::ok(),
            ts: "t".to_string(),
            agent: "agent".to_string(),
        };

        let log = ThreadLog {
            steps: vec![
                prior_review_transition,
                ThreadStep::ToolEnd {
                    tool_id: "t1".to_string(),
                    name: "staging_model".to_string(),
                    clean_name: "Staging model".to_string(),
                    args: serde_json::json!({"dataset_ids":["AwsDataCatalog.test_raw.raw_orders"]}),
                    status: "ok".to_string(),
                    payload: None,
                    ctx: None,
                    observation: react_core::session::ToolObservation::normalize(
                        serde_json::json!({"ok": true, "written_keys":["k1"]}),
                    ),
                    ts: "t".to_string(),
                    agent: "agent".to_string(),
                },
                ThreadStep::Phase {
                    phase: "cleanse_review".to_string(),
                    from_phase: Some("cleanse_validate".to_string()),
                    reason_code: Some("validate_pass".to_string()),
                    reason_detail: Some(serde_json::json!({"dbt_validate_step_idx": 1})),
                    observation: react_core::session::Observation::ok(),
                    ts: "t".to_string(),
                    agent: "agent".to_string(),
                },
            ],
            ..Default::default()
        };

        let q = DataEngineerSuite::build_review_question_with_context(
            "orig goal",
            Phase::CleanseReview,
            Some(&log),
        );
        assert!(
            q.contains("Review context"),
            "should include context header"
        );
        assert!(
            q.contains("Previous review decision"),
            "should include prior review block"
        );
        assert!(
            q.contains("What changed since previous review"),
            "should include mutation diff"
        );
        assert!(
            q.contains("staging_model"),
            "should mention mutation action"
        );
        assert!(q.contains("validate_pass"), "should include entry reason");
        assert!(
            q.contains("Original goal"),
            "should retain original goal section"
        );
    }

    #[tokio::test]
    async fn authoring_complete_reason_detail_uses_latest_log_state() {
        use react_core::session::ThreadStep;

        let sctx = SuiteCtx::default();
        let store = ThreadStore::new(
            sctx.storage.clone(),
            sctx.scope.clone(),
            sctx.keyspace.clone(),
        );
        let tid = "tid_guard_state";

        // Seed a failing validation.
        let _ = store
            .append_step(
                tid,
                ThreadStep::ToolEnd {
                    tool_id: "t2".to_string(),
                    name: "dbt_validate".to_string(),
                    clean_name: "Validate DBT".to_string(),
                    args: serde_json::json!({"build": true}),
                    status: "failed".to_string(),
                    payload: None,
                    ctx: None,
                    observation: react_core::session::ToolObservation::normalize(
                        serde_json::json!({
                            "ok": false,
                            "compile_ok": true,
                            "run_ok": false,
                            "errors": ["fail"]
                        }),
                    ),
                    ts: "t".to_string(),
                    agent: "agent".to_string(),
                },
            )
            .await;

        let before =
            DataEngineerSuite::authoring_complete_reason_detail(&store, tid, true, true).await;
        assert_eq!(
            before
                .get("guard_state")
                .and_then(|v| v.get("patched_since_fail"))
                .and_then(|v| v.as_bool()),
            Some(false)
        );

        // Then a successful dbt_files patch (even if no-op) should flip patched_since_fail.
        let _ = store
            .append_step(
                tid,
                ThreadStep::ToolEnd {
                    tool_id: "t3".to_string(),
                    name: "dbt_files".to_string(),
                    clean_name: "Patch files".to_string(),
                    args: serde_json::json!({"op": "patch"}),
                    status: "ok".to_string(),
                    payload: None,
                    ctx: None,
                    observation: react_core::session::ToolObservation::normalize(
                        serde_json::json!({
                            "ok": true,
                            "mutated": false,
                            "preview": false
                        }),
                    ),
                    ts: "t".to_string(),
                    agent: "agent".to_string(),
                },
            )
            .await;

        let after =
            DataEngineerSuite::authoring_complete_reason_detail(&store, tid, true, true).await;
        assert_eq!(
            after
                .get("guard_state")
                .and_then(|v| v.get("patched_since_fail"))
                .and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn classify_validate_failure_prefers_schema_for_precheck_and_yaml() {
        // Explicit precheck failure should be schema-class.
        assert_eq!(
            classify_validate_failure(true, None, None),
            ValidateFailureClass::SchemaOrPrecheck
        );
        // YAML/schema hints should be schema-class.
        assert_eq!(
            classify_validate_failure(false, Some("Error in models/schema.yml: duplicate definitions"), None),
            ValidateFailureClass::SchemaOrPrecheck
        );
        // Compilation errors should be SQL/runtime-class.
        assert_eq!(
            classify_validate_failure(false, Some("Compilation Error: syntax error near FROM"), None),
            ValidateFailureClass::SqlOrRuntime
        );
    }
}
