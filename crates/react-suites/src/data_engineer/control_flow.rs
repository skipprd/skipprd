use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use tokio::time::timeout;
use tracing::warn;

use react_core::agent::AgentCtx;
use react_core::control_flow::PhaseReasonCode;
use react_core::providers::DbtValidateArgs;
use react_core::session::{Observation, ThreadLog, ThreadStep, ThreadStore, ToolObservation};
use react_core::tools::Tool;

use crate::config;
use crate::data_engineer::progress_controller::ExecutionState;
use crate::data_engineer::tools::dbt_files::DbtFilesTool;
use crate::dbt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Preflight,
    CleansePlan,
    CleanseAuthor,
    CleanseValidate,
    CleanseReview,
    ModelPlan,
    ModelAuthor,
    ModelValidate,
    ModelReview,
    PublishAwaitApproval,
    Publish,
    PostPublishReview,
    Done,
}

impl Phase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Phase::Preflight => "preflight",
            Phase::CleansePlan => "cleanse_plan",
            Phase::CleanseAuthor => "cleanse_author",
            Phase::CleanseValidate => "cleanse_validate",
            Phase::CleanseReview => "cleanse_review",
            Phase::ModelPlan => "model_plan",
            Phase::ModelAuthor => "model_author",
            Phase::ModelValidate => "model_validate",
            Phase::ModelReview => "model_review",
            Phase::PublishAwaitApproval => "publish_await_approval",
            Phase::Publish => "publish",
            Phase::PostPublishReview => "post_publish_review",
            Phase::Done => "done",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "preflight" => Some(Phase::Preflight),
            "cleanse_plan" => Some(Phase::CleansePlan),
            "cleanse_author" => Some(Phase::CleanseAuthor),
            "cleanse_validate" => Some(Phase::CleanseValidate),
            "cleanse_review" => Some(Phase::CleanseReview),
            "model_plan" => Some(Phase::ModelPlan),
            "model_author" => Some(Phase::ModelAuthor),
            "model_validate" => Some(Phase::ModelValidate),
            "model_review" => Some(Phase::ModelReview),
            "publish_await_approval" => Some(Phase::PublishAwaitApproval),
            "publish" => Some(Phase::Publish),
            "post_publish_review" => Some(Phase::PostPublishReview),
            "done" => Some(Phase::Done),
            _ => None,
        }
    }
}

fn allowed_next_phases(from: Phase) -> &'static [Phase] {
    match from {
        Phase::Preflight => &[Phase::CleansePlan],
        Phase::CleansePlan => &[Phase::CleansePlan, Phase::CleanseAuthor],
        Phase::CleanseAuthor => &[Phase::CleanseAuthor, Phase::CleanseValidate, Phase::CleansePlan],
        Phase::CleanseValidate => &[
            Phase::CleanseValidate,
            Phase::CleanseAuthor,
            Phase::CleanseReview,
            Phase::CleansePlan,
        ],
        Phase::CleanseReview => &[
            Phase::CleanseReview,
            Phase::CleansePlan,
            Phase::CleanseAuthor,
            Phase::ModelPlan,
        ],
        Phase::ModelPlan => &[Phase::ModelPlan, Phase::ModelAuthor],
        Phase::ModelAuthor => &[Phase::ModelAuthor, Phase::ModelValidate, Phase::ModelPlan],
        Phase::ModelValidate => &[
            Phase::ModelValidate,
            Phase::ModelAuthor,
            Phase::ModelReview,
            Phase::ModelPlan,
        ],
        Phase::ModelReview => &[
            Phase::ModelReview,
            Phase::ModelPlan,
            Phase::ModelAuthor,
            Phase::PublishAwaitApproval,
        ],
        Phase::PublishAwaitApproval => {
            &[Phase::PublishAwaitApproval, Phase::Publish, Phase::ModelReview]
        }
        Phase::Publish => &[Phase::Publish, Phase::PostPublishReview, Phase::ModelReview],
        Phase::PostPublishReview => &[
            Phase::PostPublishReview,
            Phase::ModelPlan,
            Phase::ModelAuthor,
            Phase::PublishAwaitApproval,
            Phase::Done,
        ],
        Phase::Done => &[Phase::Done],
    }
}

fn is_annotation_reason(reason_code: Option<PhaseReasonCode>) -> bool {
    matches!(
        reason_code,
        Some(
            PhaseReasonCode::PhaseSet
                | PhaseReasonCode::ReviewProjectSummary
                | PhaseReasonCode::ReviewBatch
                | PhaseReasonCode::ReviewFinalUnify
                | PhaseReasonCode::PhaseBlocked
        )
    )
}

pub fn phase_from_log(log: Option<&ThreadLog>) -> Phase {
    let Some(log) = log else {
        return Phase::Preflight;
    };
    for step in log.steps.iter().rev() {
        if let ThreadStep::Phase { phase, .. } = step {
            if let Some(p) = Phase::from_str(phase) {
                return p;
            }
        }
    }
    Phase::Preflight
}

fn replan_backtrack_counter_cap() -> usize {
    std::env::var("AGENT_MAX_REPLAN_BACKTRACKS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(3)
        .max(2)
        .min(20)
}

fn review_patch_streak_cap(reason_code_match: PhaseReasonCode) -> usize {
    let env_key = match reason_code_match {
        PhaseReasonCode::ReviewPatchPlan => "AGENT_MAX_REVIEW_PATCH_PLAN_STREAK",
        _ => "AGENT_MAX_REVIEW_PATCH_IMPL_STREAK",
    };
    std::env::var(env_key)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(3)
        .max(1)
        .min(12)
}

pub async fn append_phase(
    store: &ThreadStore,
    thread_id: &str,
    agent: Option<String>,
    phase: Phase,
) -> Result<(), String> {
    // Best-effort: infer the previous phase if one exists; otherwise use null.
    let prev_phase = store.get(thread_id).await.ok().and_then(|log| {
        log.steps.iter().rev().find_map(|s| match s {
            ThreadStep::Phase { phase, .. } => Phase::from_str(phase),
            _ => None,
        })
    });

    append_phase_with_reason(
        store,
        thread_id,
        agent,
        prev_phase,
        phase,
        Some(PhaseReasonCode::PhaseSet),
        Some(serde_json::json!({
            "derived_from_log": prev_phase.is_some(),
        })),
    )
    .await
}

/// Append a phase marker step with explicit transition reasoning.
///
/// This is intentionally verbose: `reason_detail` is stored inline in the thread log so
/// debugging has full context without needing to cross-reference other stores.
pub async fn append_phase_with_reason(
    store: &ThreadStore,
    thread_id: &str,
    agent: Option<String>,
    from_phase: Option<Phase>,
    phase: Phase,
    reason_code: Option<PhaseReasonCode>,
    reason_detail: Option<Value>,
) -> Result<(), String> {
    if let Some(from) = from_phase {
        let is_same_phase_annotation = from == phase && is_annotation_reason(reason_code);
        if !is_same_phase_annotation && !allowed_next_phases(from).contains(&phase) {
            return Err(format!(
                "invalid_phase_transition: from='{}' to='{}' reason='{}'",
                from.as_str(),
                phase.as_str(),
                reason_code
                    .map(|c| c.as_str().to_string())
                    .unwrap_or_else(|| "none".to_string())
            ));
        }
    }

    let agent = agent.unwrap_or_else(|| "unknown".to_string());
    store
        .append_step(
            thread_id,
            ThreadStep::Phase {
                phase: phase.as_str().to_string(),
                from_phase: from_phase.map(|p| p.as_str().to_string()),
                reason_code,
                reason_detail,
                observation: Observation::ok(),
                ts: chrono::Utc::now().to_rfc3339(),
                agent,
            },
        )
        .await?;

    // Hard-cutover state control: update compact execution_state artifact on every phase transition.
    // Thread log remains audit-only; decisioning uses execution_state.
    let mut st = ExecutionState::load(store, thread_id)
        .await
        .unwrap_or_else(ExecutionState::new);
    if let Some(from) = from_phase {
        let is_backtrack =
            is_cleanse_replan_backtrack(from, phase) || is_model_replan_backtrack(from, phase);
        if is_backtrack {
            st.replan_backtracks = st
                .replan_backtracks
                .saturating_add(1)
                .min(replan_backtrack_counter_cap());
        } else if phase == Phase::Done
            || matches!(
                reason_code,
                Some(
                    PhaseReasonCode::ValidatePass
                        | PhaseReasonCode::PlanTasksDone
                        | PhaseReasonCode::NoWorkAllDone
                        | PhaseReasonCode::PublishSuccess
                        | PhaseReasonCode::PublishConfirmedSuccess
                )
            )
        {
            st.replan_backtracks = 0;
        }
    }
    st.current_phase = Some(phase);
    st.phase_reason_code = reason_code;
    st.save(store, thread_id).await?;

    Ok(())
}

#[derive(Clone, Debug, Default)]
pub struct DerivedGuardState {
    pub last_validate_failed: bool,
    pub mutated_since_fail: bool,
    /// True if at least one successful file op=patch occurred since the failing validate,
    /// even if it ended up being a no-op write (mutated=false).
    ///
    /// This is used as a conservative "we did try to apply a fix" signal to avoid deadlocking
    /// the suite purely due to mutation detection brittleness.
    pub patched_since_fail: bool,
    pub mutation_failures_since_validate: usize,
    pub probe_required: bool,
    pub probe_satisfied: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileMutationOp {
    Patch,
    Rm,
    Mv,
}

fn parse_file_mutation_op(args: &serde_json::Value) -> Option<FileMutationOp> {
    match args.get("op").and_then(|v| v.as_str()) {
        Some("patch") => Some(FileMutationOp::Patch),
        Some("rm") => Some(FileMutationOp::Rm),
        Some("mv") => Some(FileMutationOp::Mv),
        _ => None,
    }
}

fn is_mutation_step(step: &ThreadStep) -> bool {
    match step {
        ThreadStep::ToolEnd { name, args, .. } => match name.as_str() {
            "approve_and_save_artifact"
            | "approve_and_save_artifact_batch"
            | "staging_model"
            | "gold_model"
            // Deterministic authoring tools that mutate project files.
            | "apply_next_cleanse_batch"
            | "apply_next_cleanse_schema_batch"
            | "apply_next_model_batch"
            | "apply_next_model_schema_batch" => true,
            "file" => {
                parse_file_mutation_op(args).is_some()
            }
            _ => false,
        },
        _ => false,
    }
}

fn is_effective_mutation_step(step: &ThreadStep) -> bool {
    if !is_mutation_step(step) {
        return false;
    }

    let (name, _args, observation) = match step {
        ThreadStep::ToolEnd {
            name,
            args,
            observation,
            ..
        } => (name.as_str(), args, observation),
        _ => return false,
    };
    match name {
        // These tools are inherently mutating if they succeed.
        "approve_and_save_artifact" | "approve_and_save_artifact_batch" => observation.ok,
        // staging_model is only a real mutation if it wrote at least one file.
        "staging_model" | "gold_model" => observation
            .extra
            .get("written_keys")
            .and_then(|v| v.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false),
        "apply_next_cleanse_batch" | "apply_next_cleanse_schema_batch" => observation
            .extra
            .get("succeeded_dataset_ids")
            .and_then(|v| v.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false),
        "apply_next_model_batch" | "apply_next_model_schema_batch" => observation
            .extra
            .get("succeeded_item_names")
            .and_then(|v| v.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false),
        "file" => {
            if !observation.ok {
                return false;
            }
            // If results[] is present (multi-file patch), consider it authoritative.
            if let Some(arr) = observation.extra.get("results").and_then(|v| v.as_array()) {
                for it in arr {
                    if let Some(m) = it.get("mutated").and_then(|x| x.as_bool()) {
                        if m {
                            return true;
                        }
                    }
                    let base = it.get("base_sha256").and_then(|x| x.as_str()).unwrap_or("");
                    let newv = it.get("new_sha256").and_then(|x| x.as_str()).unwrap_or("");
                    if !base.is_empty() && !newv.is_empty() && base != newv {
                        return true;
                    }
                    let added = it.get("lines_added").and_then(|x| x.as_u64()).unwrap_or(0);
                    let removed = it
                        .get("lines_removed")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0);
                    if (added + removed) > 0 {
                        return true;
                    }
                }
                return false;
            }
            // Prefer explicit mutated signal when available.
            if let Some(m) = observation.extra.get("mutated").and_then(|x| x.as_bool()) {
                return m;
            }
            // Fallback: infer from postprocessed content hashes / diff stats.
            let base = observation
                .extra
                .get("base_sha256")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            let newv = observation
                .extra
                .get("new_sha256")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            if !base.is_empty() && !newv.is_empty() {
                return base != newv;
            }
            let added = observation
                .extra
                .get("lines_added")
                .and_then(|x| x.as_u64())
                .unwrap_or(0);
            let removed = observation
                .extra
                .get("lines_removed")
                .and_then(|x| x.as_u64())
                .unwrap_or(0);
            (added + removed) > 0
        }
        _ => false,
    }
}

fn looks_like_data_probe_sql(sql: &str) -> bool {
    let s = sql.trim().trim_end_matches(';').trim().to_lowercase();
    if s.is_empty() {
        return false;
    }
    // trivial validation queries should not satisfy probes
    let toks: Vec<&str> = s.split_whitespace().collect();
    if toks == ["select", "1"] {
        return false;
    }
    // allow SELECT 1 AS ok
    if toks.len() == 4 && toks[0] == "select" && toks[1] == "1" && toks[2] == "as" {
        return false;
    }
    toks.iter().any(|t| *t == "from")
}

pub fn derive_guard_state(log: Option<&ThreadLog>) -> DerivedGuardState {
    let mut out = DerivedGuardState::default();
    let Some(log) = log else { return out };

    // Find most recent dbt_validate.
    let mut last_validate_idx: Option<usize> = None;
    for (i, step) in log.steps.iter().enumerate().rev() {
        if matches!(step, ThreadStep::ToolEnd { name, .. } if name == "dbt_validate") {
            last_validate_idx = Some(i);
            break;
        }
    }
    let Some(vidx) = last_validate_idx else {
        return out;
    };
    let vstep = &log.steps[vidx];

    let (vargs, vobs) = match vstep {
        ThreadStep::ToolEnd {
            args, observation, ..
        } => (args, observation),
        _ => return out,
    };
    let ok = vobs.ok;
    let compile_ok = vobs
        .extra
        .get("compile_ok")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    let run_ok = vobs.extra.get("run_ok").and_then(|x| x.as_bool());
    let build = vargs
        .get("build")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    let run = vargs.get("run").and_then(|x| x.as_bool()).unwrap_or(false);
    let runtime_validate = build || run;

    let ok_for_clear = if runtime_validate {
        ok && compile_ok && run_ok == Some(true)
    } else {
        ok && compile_ok
    };
    out.last_validate_failed = !ok_for_clear;

    // If dbt_validate ran its internal repair loop and mutated files, treat that as a mutation
    // associated with the failing validation attempt. Without this, the suite can deadlock on
    // the "mutate after failure" guard even though dbt_validate already applied repairs.
    if out.last_validate_failed {
        if let Some(rr) = vobs.extra.get("repair_report") {
            let mut any_changed = false;
            if let Some(iters) = rr.get("iterations").and_then(|v| v.as_array()) {
                for it in iters {
                    let n = it
                        .get("llm_changed_files")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    if n > 0 {
                        any_changed = true;
                        break;
                    }
                }
            }
            if any_changed {
                out.mutated_since_fail = true;
            }
        }
    }

    // Probe requirement: compile ok but runtime failed with runtime_failures present.
    if runtime_validate && compile_ok && run_ok == Some(false) {
        let has_runtime_failures = vobs
            .extra
            .get("runtime_failures")
            .and_then(|v| v.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false);
        if has_runtime_failures {
            out.probe_required = true;
        }
    }

    // Scan forward from validate for mutations / probes.
    for step in log.steps.iter().skip(vidx + 1) {
        if is_mutation_step(step) {
            let ok = match step {
                ThreadStep::ToolEnd { observation, .. } => observation.ok,
                _ => false,
            };
            if ok {
                // Successful file patch counts as "patch applied", even if no-op.
                if let ThreadStep::ToolEnd {
                    name,
                    args,
                    observation,
                    ..
                } = step
                {
                    if name == "file" && observation.ok {
                        if parse_file_mutation_op(args).is_some() {
                            out.patched_since_fail = true;
                        }
                    }
                }
                if is_effective_mutation_step(step) {
                    out.mutated_since_fail = true;
                }
                // ok-but-ineffective (no-op) is intentionally NOT treated as a mutation or a failure.
            } else if !out.mutated_since_fail {
                // Count only consecutive tool failures until we see a successful mutation.
                out.mutation_failures_since_validate =
                    out.mutation_failures_since_validate.saturating_add(1);
            }
        }
        if out.probe_required {
            if let ThreadStep::ToolEnd {
                name,
                args,
                observation,
                ..
            } = step
            {
                if observation.ok && name == "run_sql" {
                    let sql = args.get("sql").and_then(|x| x.as_str()).unwrap_or("");
                    if looks_like_data_probe_sql(sql) {
                        out.probe_satisfied = true;
                    }
                }
            }
        }
    }

    // If probe has been satisfied, clear requirement (for gating).
    if out.probe_satisfied {
        out.probe_required = false;
    }
    out
}

/// Derive dbt `--select` terms for a fast, targeted validation pre-check based on the most recent
/// successful `file op=patch` step in the thread.
///
/// Strategy:
/// - Prefer manifest-based mapping from patched file path -> model name (when manifest is available)
/// - Fall back to dbt path selectors: `path:<rel_path>`
/// - If the patch touched global-impact files (macros/, packages.yml, dbt_project.yml), return an
///   empty list to indicate we should skip targeted validation and do full validation instead.
pub async fn derive_targeted_select_terms(ctx: &AgentCtx, log: &ThreadLog) -> Vec<String> {
    // Find the most recent successful file mutation that should influence targeted validation.
    let mut patched_paths: Vec<String> = Vec::new();
    for step in log.steps.iter().rev() {
        let ThreadStep::ToolEnd {
            name,
            args,
            observation,
            ..
        } = step
        else {
            continue;
        };
        if name != "file" || !observation.ok {
            continue;
        }
        let op = parse_file_mutation_op(args);
        // Consider patch and mv as sources of new/updated model paths.
        // (rm removes paths; targeting removed paths is usually unhelpful.)
        if !matches!(op, Some(FileMutationOp::Patch | FileMutationOp::Mv)) {
            continue;
        }

        if let Some(arr) = observation.extra.get("results").and_then(|v| v.as_array()) {
            for it in arr {
                if let Some(p) = it.get("path").and_then(|v| v.as_str()) {
                    let p = p.trim();
                    if !p.is_empty() {
                        patched_paths.push(p.to_string());
                    }
                }
            }
        }

        // If tool didn't return results[] for some reason, fall back to args.path (single-file).
        if patched_paths.is_empty() {
            match op {
                Some(FileMutationOp::Patch) => {
                    if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
                        let p = p.trim();
                        if !p.is_empty() {
                            patched_paths.push(p.to_string());
                        }
                    }
                }
                Some(FileMutationOp::Mv) => {
                    if let Some(p) = args.get("to").and_then(|v| v.as_str()) {
                        let p = p.trim();
                        if !p.is_empty() {
                            patched_paths.push(p.to_string());
                        }
                    }
                }
                _ => {}
            }
        }
        break;
    }

    if patched_paths.is_empty() {
        return Vec::new();
    }

    // Global-impact files: skip targeted checks (selection isn't reliable / can be too broad).
    for p in patched_paths.iter() {
        let pl = p.to_ascii_lowercase();
        if pl == "packages.yml" || pl == "dbt_project.yml" || pl.starts_with("macros/") {
            return Vec::new();
        }
    }

    // Only target model SQL paths (dbt path selector expects project-relative paths).
    let mut model_paths: Vec<String> = patched_paths
        .into_iter()
        .filter(|p| p.starts_with("models/") && p.ends_with(".sql"))
        .collect();
    model_paths.sort();
    model_paths.dedup();

    if model_paths.is_empty() {
        return Vec::new();
    }

    // Attempt manifest mapping (best-effort).
    let mut path_to_model_name: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    {
        let base = ctx.keyspace.dbt_prefix(&ctx.scope);
        let base = base.trim_end_matches('/').to_string() + "/";
        let manifest_key = format!("{}target/manifest.json", base);
        if let Ok(bytes) = ctx.storage.get_bytes(&manifest_key).await {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                if let Some(nodes) = v.get("nodes").and_then(|n| n.as_object()) {
                    for (_uid, node) in nodes.iter() {
                        let rt = node
                            .get("resource_type")
                            .and_then(|x| x.as_str())
                            .unwrap_or("");
                        if rt != "model" {
                            continue;
                        }
                        let fp = node
                            .get("original_file_path")
                            .and_then(|x| x.as_str())
                            .or_else(|| node.get("path").and_then(|x| x.as_str()))
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        if fp.is_empty() {
                            continue;
                        }
                        if !model_paths.iter().any(|p| p == &fp) {
                            continue;
                        }
                        let name = node
                            .get("name")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .trim();
                        if !name.is_empty() {
                            path_to_model_name.insert(fp, name.to_string());
                        }
                    }
                }
            }
        }
    }

    // Default: include parents to catch upstream dependency breakage early.
    let include_parents = true;
    let mut out: Vec<String> = Vec::new();
    for p in model_paths.iter() {
        let sel = if let Some(name) = path_to_model_name.get(p) {
            name.clone()
        } else {
            format!("path:{}", p)
        };
        if include_parents {
            out.push(format!("+{}", sel));
        } else {
            out.push(sel);
        }
    }
    out.sort();
    out.dedup();
    out
}

fn is_cleanse_replan_backtrack(from: Phase, to: Phase) -> bool {
    matches!(from, Phase::CleanseValidate | Phase::CleanseReview)
        && matches!(to, Phase::CleansePlan | Phase::CleanseAuthor)
}

fn is_model_replan_backtrack(from: Phase, to: Phase) -> bool {
    matches!(
        from,
        Phase::ModelValidate | Phase::ModelReview | Phase::PostPublishReview
    ) && matches!(to, Phase::ModelPlan | Phase::ModelAuthor)
}

/// Count validate/review -> plan/author backtracks for the active track.
///
/// This is used to stop threads that repeatedly loop without meaningful phase progress.
pub fn replan_backtrack_count_for_phase(log: Option<&ThreadLog>, phase: Phase) -> usize {
    let Some(log) = log else { return 0 };
    // Guard tuning: only count loopbacks since the most recent *successful* dbt_validate.
    // Once validate succeeds, we consider the backtrack loop "resolved" for hard-stop purposes.
    let mut last_successful_validate_idx: Option<usize> = None;
    let mut cur_phase: Option<Phase> = None;
    for (i, step) in log.steps.iter().enumerate() {
        match step {
            ThreadStep::Phase { phase, .. } => {
                cur_phase = Phase::from_str(phase);
            }
            ThreadStep::ToolEnd {
                name, observation, ..
            } => {
                if name == "dbt_validate"
                    && observation.ok
                    && matches!(
                        cur_phase,
                        Some(Phase::CleanseValidate | Phase::ModelValidate)
                    )
                {
                    last_successful_validate_idx = Some(i);
                }
            }
            _ => {}
        }
    }
    let start_idx = last_successful_validate_idx.unwrap_or(0);

    let mut count = 0usize;
    let cap = replan_backtrack_counter_cap();
    for (i, step) in log.steps.iter().enumerate() {
        if i <= start_idx {
            continue;
        }
        let ThreadStep::Phase {
            phase: to_phase,
            from_phase: Some(from_phase),
            ..
        } = step
        else {
            continue;
        };
        let Some(from) = Phase::from_str(from_phase) else {
            continue;
        };
        let Some(to) = Phase::from_str(to_phase) else {
            continue;
        };
        let hit = match phase {
            Phase::CleansePlan
            | Phase::CleanseAuthor
            | Phase::CleanseValidate
            | Phase::CleanseReview => is_cleanse_replan_backtrack(from, to),
            Phase::ModelPlan
            | Phase::ModelAuthor
            | Phase::ModelValidate
            | Phase::ModelReview
            | Phase::PublishAwaitApproval
            | Phase::Publish
            | Phase::PostPublishReview
            | Phase::Done => is_model_replan_backtrack(from, to),
            Phase::Preflight => false,
        };
        if hit {
            count = count.saturating_add(1);
            if count >= cap {
                return cap;
            }
        }
    }
    count
}

fn review_patch_streak(
    log: Option<&ThreadLog>,
    review_phase: Phase,
    reason_code_match: PhaseReasonCode,
    expected_back_to: Phase,
) -> usize {
    let Some(log) = log else { return 0 };
    let cap = review_patch_streak_cap(reason_code_match);

    let mut streak = 0usize;
    for step in log.steps.iter().rev() {
        let ThreadStep::Phase {
            phase: to_phase,
            from_phase: Some(from_phase),
            reason_code,
            ..
        } = step
        else {
            continue;
        };
        let Some(from) = Phase::from_str(from_phase) else {
            continue;
        };
        let Some(to) = Phase::from_str(to_phase) else {
            continue;
        };
        if from != review_phase {
            continue;
        }
        if matches!(reason_code, Some(code) if *code == reason_code_match) && to == expected_back_to
        {
            streak = streak.saturating_add(1);
            if streak >= cap {
                return cap;
            }
            continue;
        }
        // Any other decision emitted by this review phase ends the streak window.
        break;
    }
    streak
}

/// Count consecutive review->plan loopbacks caused by `review_patch_plan`.
pub fn review_patch_plan_streak(log: Option<&ThreadLog>, review_phase: Phase) -> usize {
    let expected_back_to = match review_phase {
        Phase::CleanseReview => Phase::CleansePlan,
        Phase::ModelReview | Phase::PostPublishReview => Phase::ModelPlan,
        _ => return 0,
    };
    review_patch_streak(
        log,
        review_phase,
        PhaseReasonCode::ReviewPatchPlan,
        expected_back_to,
    )
}

/// Count consecutive review->author loopbacks caused by `review_patch_impl`.
///
/// This is intentionally review-phase scoped and is used as a secondary guard when
/// validate keeps passing but review repeatedly requests more implementation patching.
pub fn review_patch_impl_streak(log: Option<&ThreadLog>, review_phase: Phase) -> usize {
    let expected_back_to = match review_phase {
        Phase::CleanseReview => Phase::CleanseAuthor,
        Phase::ModelReview | Phase::PostPublishReview => Phase::ModelAuthor,
        _ => return 0,
    };
    review_patch_streak(
        log,
        review_phase,
        PhaseReasonCode::ReviewPatchImpl,
        expected_back_to,
    )
}

#[derive(Clone, Debug)]
pub enum AuthoringGate {
    Allow,
    Block { reason: String },
}

fn phase_start_idx(log: &ThreadLog, phase: Phase) -> Option<usize> {
    for (i, step) in log.steps.iter().enumerate().rev() {
        if let ThreadStep::Phase { phase: p, .. } = step {
            let got = Phase::from_str(p);
            if got == Some(phase) {
                return Some(i);
            }
        }
    }
    None
}

fn unresolved_mutation_failures_in_phase(log: &ThreadLog, phase: Phase) -> Vec<String> {
    // Collect failures since the last successful mutation in the current phase.
    // If a successful mutation occurs, it "clears" prior failures.
    let Some(start) = phase_start_idx(log, phase) else {
        return Vec::new();
    };

    let mut failures: Vec<String> = Vec::new();
    for step in log.steps.iter().skip(start + 1) {
        if !is_mutation_step(step) {
            continue;
        }
        let (name, obs) = match step {
            ThreadStep::ToolEnd {
                name, observation, ..
            } => (name.as_str(), observation),
            _ => continue,
        };
        let ok = obs.ok;
        if ok {
            if is_effective_mutation_step(step) {
                failures.clear();
            }
            // ok-but-ineffective is not a failure; it just doesn't clear prior failures.
            continue;
        }
        let err = obs
            .first_error_or_context()
            .unwrap_or_else(|| "no error details were captured".to_string());
        failures.push(format!("{}: {}", name, err));
        if failures.len() >= 10 {
            break;
        }
    }
    failures
}

/// Single source of truth for whether we may advance from an authoring phase into a validate phase.
///
/// This intentionally enforces suite-level invariants (mutation/probe requirements) in code,
/// rather than relying on prompt-only instructions.
pub fn gate_authoring_to_validate(log: Option<&ThreadLog>) -> AuthoringGate {
    let g = derive_guard_state(log);
    if g.last_validate_failed && !(g.mutated_since_fail || g.patched_since_fail) {
        if g.mutation_failures_since_validate >= 3 {
            return AuthoringGate::Block {
                reason: format!(
                    "I tried to apply a mutating fix after a failed dbt_validate, but the mutation step failed {} times in a row (often due to tool timeouts or storage write failures).\n\nPlease check:\n- The runtime can write DBT files to storage (S3 prefix/permissions)\n- The agent tool timeout is sufficient for your environment\n\nThen retry. If you want a quick deterministic fix path, use `file op=patch` to edit the failing model SQL directly.",
                    g.mutation_failures_since_validate
                ),
            };
        }
        return AuthoringGate::Block {
            reason: "A DBT validation previously failed and no successful mutation has been recorded since that failure. Apply a mutating fix (e.g. file op=patch or staging_model) before re-validating."
                .to_string(),
        };
    }
    if g.probe_required && !g.probe_satisfied {
        return AuthoringGate::Block {
            reason: "Runtime validation previously failed after compile and a data probe is required. Run meaningful run_sql probes (not SELECT 1) to diagnose the failing relation before re-validating."
                .to_string(),
        };
    }
    AuthoringGate::Allow
}

/// Gate authoring completion itself (before advancing phases) on unresolved tool/mutation failures
/// in the current authoring phase. This prevents the suite from moving forward after a failing
/// mutation (e.g., staging_model/tool timeouts), even if the agent produced a Final response.
pub fn gate_authoring_completion(log: Option<&ThreadLog>, phase: Phase) -> AuthoringGate {
    let Some(log) = log else {
        return AuthoringGate::Allow;
    };
    match phase {
        Phase::CleanseAuthor | Phase::ModelAuthor => {}
        _ => return AuthoringGate::Allow,
    }

    let failures = unresolved_mutation_failures_in_phase(log, phase);
    if failures.is_empty() {
        return AuthoringGate::Allow;
    }
    let mut msg = String::new();
    msg.push_str("Unresolved mutation/tool failures occurred in this authoring phase. Fix these before advancing to validation:\n");
    for f in failures.iter().take(6) {
        msg.push_str("- ");
        msg.push_str(f);
        msg.push('\n');
    }
    AuthoringGate::Block {
        reason: msg.trim().to_string(),
    }
}

pub fn gate_author_phase_execution_cleanse(
    plan: &crate::data_engineer::plan::CleansePlan,
) -> AuthoringGate {
    let issues = crate::data_engineer::plan::cleanse_executable_plan_issues(plan);
    if issues.is_empty() {
        return AuthoringGate::Allow;
    }
    let mut msg = String::from(
        "Approved cleanse plan is not executable. Re-enter planning before authoring:\n",
    );
    for issue in issues.iter().take(8) {
        msg.push_str("- ");
        msg.push_str(issue);
        msg.push('\n');
    }
    AuthoringGate::Block {
        reason: msg.trim().to_string(),
    }
}

pub fn gate_author_phase_execution_model(
    plan: &crate::data_engineer::plan::ModelPlan,
) -> AuthoringGate {
    let issues = crate::data_engineer::plan::model_executable_plan_issues(plan);
    if issues.is_empty() {
        return AuthoringGate::Allow;
    }
    let mut msg = String::from(
        "Approved model plan is not executable. Re-enter planning before authoring:\n",
    );
    for issue in issues.iter().take(8) {
        msg.push_str("- ");
        msg.push_str(issue);
        msg.push('\n');
    }
    AuthoringGate::Block {
        reason: msg.trim().to_string(),
    }
}

pub struct DeterministicDbtValidateOnce;

impl DeterministicDbtValidateOnce {
    pub async fn run(
        ctx: &AgentCtx,
        build: bool,
        run: bool,
        dataset_ids: Option<&[String]>,
    ) -> Result<Value, String> {
        let dbt = ctx
            .dbt
            .as_ref()
            .ok_or_else(|| "dbt provider missing".to_string())?;
        let Some(cfg) = config::resolved_config_from_ctx(ctx) else {
            return Err(
                "resolved_config missing (needed to generate profiles.yml deterministically)"
                    .to_string(),
            );
        };
        let threads = ctx.query.as_ref().map(|q| q.max_concurrency());
        let gen = dbt::profile::generate_profiles_yml(cfg, threads)?;
        let td = tempfile::tempdir().map_err(|e| e.to_string())?;
        let profiles_dir = td.path().to_string_lossy().to_string();
        let profiles_path = td.path().join("profiles.yml");
        std::fs::write(&profiles_path, gen.profiles_yml.as_bytes()).map_err(|e| e.to_string())?;

        let res = dbt
            .validate_project(
                &ctx.scope,
                &DbtValidateArgs {
                    project_name: "data_engineer".to_string(),
                    profiles_dir: Some(profiles_dir),
                    target: gen.target,
                    run,
                    build,
                    select: None,
                    exclude: None,
                },
            )
            .await?;

        let mut v = serde_json::to_value(res).unwrap_or_else(
            |_| serde_json::json!({"ok": false, "error": "failed to serialize result"}),
        );
        if let Some(obj) = v.as_object_mut() {
            obj.insert(
                "dialect".to_string(),
                serde_json::json!(
                    crate::data_engineer::dbt_repair::remediate::active_provider_dialect(cfg)
                ),
            );
            // Keep parity with dbt_validate tool output, but do NOT mutate/repair here.
            let rf = crate::data_engineer::dbt_error::extract_runtime_failures_from_logs(
                &obj.get("logs").cloned().unwrap_or(Value::Null),
            );
            obj.insert("runtime_failures".to_string(), serde_json::json!(rf));
            let ok = obj.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
            if !ok {
                let errors: Vec<String> = obj
                    .get("errors")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(|s| s.to_string()))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let logs = obj.get("logs").cloned().unwrap_or(Value::Null);
                if let Ok(sum) = crate::data_engineer::dbt_error::summarize_dbt_failure_llm(
                    ctx.llm.as_ref(),
                    &errors,
                    &logs,
                    &rf,
                    2000,
                ) {
                    obj.insert("error_summary".to_string(), serde_json::json!(sum.summary));
                    obj.insert(
                        "failing_nodes".to_string(),
                        serde_json::json!(sum.failing_nodes),
                    );
                    obj.insert(
                        "suggested_next_files".to_string(),
                        serde_json::json!(sum.suggested_next_files),
                    );
                }
            }
            if let Some(ds) = dataset_ids {
                obj.insert("dataset_ids".to_string(), serde_json::json!(ds));
            }
        }
        Ok(v)
    }
}

/// Deterministic, fail-fast dbt validation for a targeted selection set (no repair loop).
///
/// Intended for quick pre-checks after patching a small set of dbt files/models.
pub struct DeterministicDbtValidateTargetedOnce;

impl DeterministicDbtValidateTargetedOnce {
    pub async fn run(
        ctx: &AgentCtx,
        select_terms: &[String],
        build: bool,
        run: bool,
    ) -> Result<Value, String> {
        let dbt = ctx
            .dbt
            .as_ref()
            .ok_or_else(|| "dbt provider missing".to_string())?;
        let Some(cfg) = config::resolved_config_from_ctx(ctx) else {
            return Err(
                "resolved_config missing (needed to generate profiles.yml deterministically)"
                    .to_string(),
            );
        };
        let threads = ctx.query.as_ref().map(|q| q.max_concurrency());
        let gen = dbt::profile::generate_profiles_yml(cfg, threads)?;
        let td = tempfile::tempdir().map_err(|e| e.to_string())?;
        let profiles_dir = td.path().to_string_lossy().to_string();
        let profiles_path = td.path().join("profiles.yml");
        std::fs::write(&profiles_path, gen.profiles_yml.as_bytes()).map_err(|e| e.to_string())?;

        let res = dbt
            .validate_project(
                &ctx.scope,
                &DbtValidateArgs {
                    project_name: "data_engineer".to_string(),
                    profiles_dir: Some(profiles_dir),
                    target: gen.target,
                    run,
                    build,
                    select: Some(select_terms.to_vec()),
                    exclude: None,
                },
            )
            .await?;

        let mut v = serde_json::to_value(res).unwrap_or_else(
            |_| serde_json::json!({"ok": false, "error": "failed to serialize result"}),
        );
        if let Some(obj) = v.as_object_mut() {
            obj.insert(
                "dialect".to_string(),
                serde_json::json!(
                    crate::data_engineer::dbt_repair::remediate::active_provider_dialect(cfg)
                ),
            );
            // Keep parity with dbt_validate tool output, but do NOT mutate/repair here.
            let rf = crate::data_engineer::dbt_error::extract_runtime_failures_from_logs(
                &obj.get("logs").cloned().unwrap_or(Value::Null),
            );
            obj.insert("runtime_failures".to_string(), serde_json::json!(rf));
            obj.insert("select".to_string(), serde_json::json!(select_terms));
            let ok = obj.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
            if !ok {
                let errors: Vec<String> = obj
                    .get("errors")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(|s| s.to_string()))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let logs = obj.get("logs").cloned().unwrap_or(Value::Null);
                if let Ok(sum) = crate::data_engineer::dbt_error::summarize_dbt_failure_llm(
                    ctx.llm.as_ref(),
                    &errors,
                    &logs,
                    &rf,
                    2000,
                ) {
                    obj.insert("error_summary".to_string(), serde_json::json!(sum.summary));
                    obj.insert(
                        "failing_nodes".to_string(),
                        serde_json::json!(sum.failing_nodes),
                    );
                    obj.insert(
                        "suggested_next_files".to_string(),
                        serde_json::json!(sum.suggested_next_files),
                    );
                }
            }
        }
        Ok(v)
    }
}

pub async fn call_and_record_tool(
    store: &ThreadStore,
    thread_id: &str,
    agent: Option<String>,
    tool: &dyn Tool,
    args: Value,
    ctx: &AgentCtx,
    timeout_secs: u64,
) -> Value {
    fn clean_tool_name(name: &str, args: &Value) -> String {
        match name {
            "file" => {
                let op = args.get("op").and_then(|v| v.as_str()).unwrap_or("");
                match op {
                    "get" => {
                        let p = args
                            .get("path")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .trim();
                        if !p.is_empty() {
                            return format!("Read {p}");
                        }
                        "Read file".to_string()
                    }
                    "list" => {
                        let p = args
                            .get("prefix")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .trim();
                        if !p.is_empty() {
                            return format!("List {p}");
                        }
                        "List files".to_string()
                    }
                    "patch" => {
                        let p = args
                            .get("path")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .trim();
                        if !p.is_empty() {
                            return format!("Patch {p}");
                        }
                        "Patch file".to_string()
                    }
                    _ => {
                        if !op.is_empty() {
                            return format!("file {op}");
                        }
                        "file".to_string()
                    }
                }
            }
            "json_file" => {
                let p = args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim();
                if !p.is_empty() {
                    format!("JSON {p}")
                } else {
                    "JSON file".to_string()
                }
            }
            "sql_schema" => {
                let t = args
                    .get("table")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim();
                if !t.is_empty() {
                    format!("Describe {t}")
                } else {
                    "List tables".to_string()
                }
            }
            "sql_stats" => {
                let t = args
                    .get("table")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim();
                if !t.is_empty() {
                    format!("Stats {t}")
                } else {
                    "Stats".to_string()
                }
            }
            "sql_sample" => {
                let t = args
                    .get("table")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim();
                if !t.is_empty() {
                    format!("Sample {t}")
                } else {
                    "Sample".to_string()
                }
            }
            "run_sql" => "Run SQL".to_string(),
            other => other.replace('_', " "),
        }
    }

    let agent = agent.unwrap_or_else(|| "unknown".to_string());
    let clean_name = clean_tool_name(tool.name(), &args);
    let tool_id = uuid::Uuid::new_v4().to_string();
    let ts_start = chrono::Utc::now().to_rfc3339();
    if let Err(e) = store
        .append_step(
            thread_id,
            ThreadStep::ToolStart {
                tool_id: tool_id.clone(),
                name: tool.name().to_string(),
                clean_name: clean_name.clone(),
                args: args.clone(),
                status: "running".to_string(),
                payload: None,
                ctx: ctx.exec_ctx.clone(),
                ts: ts_start,
                agent: agent.clone(),
            },
        )
        .await
    {
        warn!("failed to append tool start step: {}", e);
    }

    let raw = match timeout(
        Duration::from_secs(timeout_secs.max(1)),
        tool.call(args.clone(), ctx),
    )
    .await
    {
        Ok(r) => r.unwrap_or_else(|e| serde_json::json!({"ok": false, "errors": [e]})),
        Err(_) => serde_json::json!({"ok": false, "errors": ["tool timeout"]}),
    };
    let obs = ToolObservation::normalize(raw.clone());
    let status = if obs.ok {
        "ok".to_string()
    } else {
        "failed".to_string()
    };
    let payload = obs
        .extra
        .get("payload")
        .cloned()
        .or_else(|| obs.extra.get("ui_payload").cloned());
    if let Err(e) = store
        .append_step(
            thread_id,
            ThreadStep::ToolEnd {
                tool_id,
                name: tool.name().to_string(),
                clean_name,
                args,
                status,
                payload,
                ctx: ctx.exec_ctx.clone(),
                observation: obs,
                ts: chrono::Utc::now().to_rfc3339(),
                agent,
            },
        )
        .await
    {
        warn!("failed to append tool end step: {}", e);
    }
    raw
}

/// Deterministic authoring invariant: ensure there is at least one model SQL file in `models/`.
pub async fn invariant_has_any_models(ctx: &AgentCtx) -> Result<bool, String> {
    let tool = DbtFilesTool { datasets: None };
    let obs = tool
        .call(
            serde_json::json!({"op":"list","prefix":"models/","limit":500}),
            ctx,
        )
        .await
        .unwrap_or_else(|e| serde_json::json!({"ok": false, "error": e}));
    if obs.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        warn!("file list failed: {:?}", obs.get("error"));
        return Ok(false);
    }
    let items = obs
        .get("items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut n_sql = 0usize;
    for it in items {
        let Some(p) = it.get("path").and_then(|v| v.as_str()) else {
            continue;
        };
        if p.starts_with("models/") && p.ends_with(".sql") && !p.contains("/_versions/") {
            n_sql += 1;
        }
    }
    Ok(n_sql > 0)
}

/// Deterministic invariant: dbt_project.yml exists in the scoped dbt project.
pub async fn invariant_has_dbt_project(ctx: &AgentCtx) -> Result<bool, String> {
    let tool = DbtFilesTool { datasets: None };
    let obs = tool
        .call(
            serde_json::json!({"op":"get","path":"dbt_project.yml","max_chars":2000}),
            ctx,
        )
        .await
        .unwrap_or_else(|e| serde_json::json!({"ok": false, "error": e}));
    Ok(obs.get("ok").and_then(|v| v.as_bool()) == Some(true))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_reason_code(s: &str) -> Option<PhaseReasonCode> {
        match s {
            "phase_set" => Some(PhaseReasonCode::PhaseSet),
            "preflight_start" => Some(PhaseReasonCode::PreflightStart),
            "preflight_ok" => Some(PhaseReasonCode::PreflightOk),
            "plan_approved" => Some(PhaseReasonCode::PlanApproved),
            "plan_auto_approved" => Some(PhaseReasonCode::PlanAutoApproved),
            "plan_already_approved" => Some(PhaseReasonCode::PlanAlreadyApproved),
            "plan_missing" => Some(PhaseReasonCode::PlanMissing),
            "plan_not_approved" => Some(PhaseReasonCode::PlanNotApproved),
            "plan_invalid_empty" => Some(PhaseReasonCode::PlanInvalidEmpty),
            "plan_pruned_empty" => Some(PhaseReasonCode::PlanPrunedEmpty),
            "plan_semantic_invalid" => Some(PhaseReasonCode::PlanSemanticInvalid),
            "work_group_validate" => Some(PhaseReasonCode::WorkGroupValidate),
            "plan_tasks_done" => Some(PhaseReasonCode::PlanTasksDone),
            "no_work_all_done" => Some(PhaseReasonCode::NoWorkAllDone),
            "authoring_complete" => Some(PhaseReasonCode::AuthoringComplete),
            "precheck_failed" => Some(PhaseReasonCode::PrecheckFailed),
            "validate_pass" => Some(PhaseReasonCode::ValidatePass),
            "validate_fail" => Some(PhaseReasonCode::ValidateFail),
            "review_proceed" => Some(PhaseReasonCode::ReviewProceed),
            "review_patch_plan" => Some(PhaseReasonCode::ReviewPatchPlan),
            "review_patch_impl" => Some(PhaseReasonCode::ReviewPatchImpl),
            "publish_success" => Some(PhaseReasonCode::PublishSuccess),
            "publish_fail" => Some(PhaseReasonCode::PublishFail),
            "publish_confirmed_success" => Some(PhaseReasonCode::PublishConfirmedSuccess),
            "publish_confirmed_fail" => Some(PhaseReasonCode::PublishConfirmedFail),
            "user_approved_publish" => Some(PhaseReasonCode::UserApprovedPublish),
            "phase_blocked" => Some(PhaseReasonCode::PhaseBlocked),
            _ => None,
        }
    }

    fn step(action: &str, args: Value, observation: Value) -> ThreadStep {
        let ts = chrono::Utc::now().to_rfc3339();
        match action {
            "phase" => ThreadStep::Phase {
                phase: args
                    .get("phase")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                from_phase: args
                    .get("from_phase")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                reason_code: args
                    .get("reason_code")
                    .and_then(|v| v.as_str())
                    .and_then(parse_reason_code),
                reason_detail: args.get("reason_detail").cloned(),
                observation: Observation::ok(),
                ts,
                agent: "test".to_string(),
            },
            _ => {
                let obs = ToolObservation::normalize(observation);
                ThreadStep::ToolEnd {
                    tool_id: "t".to_string(),
                    name: action.to_string(),
                    clean_name: action.to_string(),
                    args,
                    status: if obs.ok {
                        "ok".to_string()
                    } else {
                        "failed".to_string()
                    },
                    payload: None,
                    ctx: None,
                    observation: obs,
                    ts,
                    agent: "test".to_string(),
                }
            }
        }
    }

    #[test]
    fn phase_is_derived_from_last_phase_step() {
        let log = ThreadLog {
            steps: vec![
                step(
                    "phase",
                    serde_json::json!({"phase":"preflight"}),
                    serde_json::json!({"ok":true}),
                ),
                step(
                    "phase",
                    serde_json::json!({"phase":"model_author"}),
                    serde_json::json!({"ok":true}),
                ),
            ],
            ..Default::default()
        };
        assert_eq!(phase_from_log(Some(&log)), Phase::ModelAuthor);
    }

    #[test]
    fn replan_backtrack_count_counts_cleanse_loopbacks_only() {
        let log = ThreadLog {
            steps: vec![
                step(
                    "phase",
                    serde_json::json!({"phase":"cleanse_author","from_phase":"cleanse_plan"}),
                    serde_json::json!({"ok":true}),
                ),
                step(
                    "phase",
                    serde_json::json!({"phase":"cleanse_validate","from_phase":"cleanse_author"}),
                    serde_json::json!({"ok":true}),
                ),
                step(
                    "phase",
                    serde_json::json!({"phase":"cleanse_author","from_phase":"cleanse_validate"}),
                    serde_json::json!({"ok":true}),
                ),
                step(
                    "phase",
                    serde_json::json!({"phase":"cleanse_review","from_phase":"cleanse_validate"}),
                    serde_json::json!({"ok":true}),
                ),
                step(
                    "phase",
                    serde_json::json!({"phase":"cleanse_plan","from_phase":"cleanse_review"}),
                    serde_json::json!({"ok":true}),
                ),
                // Forward progression to model should NOT be counted as a cleanse loopback.
                step(
                    "phase",
                    serde_json::json!({"phase":"model_plan","from_phase":"cleanse_review"}),
                    serde_json::json!({"ok":true}),
                ),
            ],
            ..Default::default()
        };
        assert_eq!(
            replan_backtrack_count_for_phase(Some(&log), Phase::CleanseAuthor),
            2
        );
    }

    #[test]
    fn replan_backtrack_count_counts_model_loopbacks_only() {
        let log = ThreadLog {
            steps: vec![
                step(
                    "phase",
                    serde_json::json!({"phase":"model_author","from_phase":"model_plan"}),
                    serde_json::json!({"ok":true}),
                ),
                step(
                    "phase",
                    serde_json::json!({"phase":"model_validate","from_phase":"model_author"}),
                    serde_json::json!({"ok":true}),
                ),
                step(
                    "phase",
                    serde_json::json!({"phase":"model_author","from_phase":"model_validate"}),
                    serde_json::json!({"ok":true}),
                ),
                step(
                    "phase",
                    serde_json::json!({"phase":"model_review","from_phase":"model_validate"}),
                    serde_json::json!({"ok":true}),
                ),
                step(
                    "phase",
                    serde_json::json!({"phase":"model_plan","from_phase":"model_review"}),
                    serde_json::json!({"ok":true}),
                ),
            ],
            ..Default::default()
        };
        assert_eq!(
            replan_backtrack_count_for_phase(Some(&log), Phase::ModelAuthor),
            2
        );
        assert_eq!(
            replan_backtrack_count_for_phase(Some(&log), Phase::CleanseAuthor),
            0
        );
    }

    #[test]
    fn replan_backtrack_count_resets_after_successful_dbt_validate() {
        let log = ThreadLog {
            steps: vec![
                step(
                    "phase",
                    serde_json::json!({"phase":"model_author","from_phase":"model_plan"}),
                    serde_json::json!({"ok":true}),
                ),
                step(
                    "phase",
                    serde_json::json!({"phase":"model_validate","from_phase":"model_author"}),
                    serde_json::json!({"ok":true}),
                ),
                // Loopback (validate -> author) prior to a successful validate.
                step(
                    "phase",
                    serde_json::json!({"phase":"model_author","from_phase":"model_validate"}),
                    serde_json::json!({"ok":true}),
                ),
                step(
                    "phase",
                    serde_json::json!({"phase":"model_validate","from_phase":"model_author"}),
                    serde_json::json!({"ok":true}),
                ),
                // Successful validate should reset the hard-stop counter.
                step(
                    "dbt_validate",
                    serde_json::json!({}),
                    serde_json::json!({"ok":true}),
                ),
                step(
                    "phase",
                    serde_json::json!({"phase":"publish_await_approval","from_phase":"model_validate"}),
                    serde_json::json!({"ok":true}),
                ),
            ],
            ..Default::default()
        };
        assert_eq!(
            replan_backtrack_count_for_phase(Some(&log), Phase::PublishAwaitApproval),
            0
        );
    }

    #[test]
    fn review_patch_plan_streak_counts_consecutive_review_loopbacks() {
        let log = ThreadLog {
            steps: vec![
                ThreadStep::Phase {
                    phase: "cleanse_plan".to_string(),
                    from_phase: Some("cleanse_review".to_string()),
                    reason_code: Some(PhaseReasonCode::ReviewPatchPlan),
                    reason_detail: None,
                    observation: react_core::session::Observation::ok(),
                    ts: "2026-01-01T00:00:00Z".to_string(),
                    agent: "test".to_string(),
                },
                ThreadStep::Phase {
                    phase: "cleanse_plan".to_string(),
                    from_phase: Some("cleanse_review".to_string()),
                    reason_code: Some(PhaseReasonCode::ReviewPatchPlan),
                    reason_detail: None,
                    observation: react_core::session::Observation::ok(),
                    ts: "2026-01-01T00:00:01Z".to_string(),
                    agent: "test".to_string(),
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            review_patch_plan_streak(Some(&log), Phase::CleanseReview),
            2
        );
    }

    #[test]
    fn review_patch_plan_streak_stops_on_non_patch_plan_transition() {
        let log = ThreadLog {
            steps: vec![
                ThreadStep::Phase {
                    phase: "cleanse_plan".to_string(),
                    from_phase: Some("cleanse_review".to_string()),
                    reason_code: Some(PhaseReasonCode::ReviewPatchPlan),
                    reason_detail: None,
                    observation: react_core::session::Observation::ok(),
                    ts: "2026-01-01T00:00:00Z".to_string(),
                    agent: "test".to_string(),
                },
                ThreadStep::Phase {
                    phase: "model_plan".to_string(),
                    from_phase: Some("cleanse_review".to_string()),
                    reason_code: Some(PhaseReasonCode::ReviewProceed),
                    reason_detail: None,
                    observation: react_core::session::Observation::ok(),
                    ts: "2026-01-01T00:00:01Z".to_string(),
                    agent: "test".to_string(),
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            review_patch_plan_streak(Some(&log), Phase::CleanseReview),
            0
        );
    }

    #[test]
    fn guard_blocks_validate_until_mutation_after_failure() {
        let log = ThreadLog {
            steps: vec![
                step(
                    "dbt_validate",
                    serde_json::json!({"build": true}),
                    serde_json::json!({"ok": false, "compile_ok": false, "run_ok": false, "errors":["x"]}),
                ),
                // no mutation after
            ],
            ..Default::default()
        };
        let g = derive_guard_state(Some(&log));
        assert!(g.last_validate_failed);
        assert!(!g.mutated_since_fail);
        assert_eq!(g.mutation_failures_since_validate, 0);
    }

    #[test]
    fn guard_clears_block_after_mutation() {
        let log = ThreadLog {
            steps: vec![
                step(
                    "dbt_validate",
                    serde_json::json!({"build": true}),
                    serde_json::json!({"ok": false, "compile_ok": false, "run_ok": false, "errors":["x"]}),
                ),
                step(
                    "file",
                    serde_json::json!({"op":"patch","path":"models/a.sql","patch_text":"@@ ... @@\n+select 1\n"}),
                    serde_json::json!({"ok": true, "mutated": true}),
                ),
            ],
            ..Default::default()
        };
        let g = derive_guard_state(Some(&log));
        assert!(g.last_validate_failed);
        assert!(g.mutated_since_fail);
    }

    #[test]
    fn guard_clears_block_after_dbt_files_rm_mutation() {
        let log = ThreadLog {
            steps: vec![
                step(
                    "dbt_validate",
                    serde_json::json!({"build": true}),
                    serde_json::json!({"ok": false, "compile_ok": false, "run_ok": false, "errors":["x"]}),
                ),
                step(
                    "file",
                    serde_json::json!({"op":"rm","path":"models/a.sql"}),
                    serde_json::json!({"ok": true, "mutated": true, "results":[{"path":"models/a.sql","mutated":true}]}),
                ),
            ],
            ..Default::default()
        };
        let g = derive_guard_state(Some(&log));
        assert!(g.last_validate_failed);
        assert!(g.mutated_since_fail);
    }

    #[test]
    fn guard_clears_block_after_dbt_files_mv_mutation() {
        let log = ThreadLog {
            steps: vec![
                step(
                    "dbt_validate",
                    serde_json::json!({"build": true}),
                    serde_json::json!({"ok": false, "compile_ok": false, "run_ok": false, "errors":["x"]}),
                ),
                step(
                    "file",
                    serde_json::json!({"op":"mv","from":"models/a.sql","to":"models/b.sql"}),
                    serde_json::json!({"ok": true, "mutated": true, "results":[{"path":"models/b.sql","mutated":true}]}),
                ),
            ],
            ..Default::default()
        };
        let g = derive_guard_state(Some(&log));
        assert!(g.last_validate_failed);
        assert!(g.mutated_since_fail);
    }

    #[test]
    fn guard_marks_patched_since_fail_on_successful_noop_patch() {
        let log = ThreadLog {
            steps: vec![
                step(
                    "dbt_validate",
                    serde_json::json!({"build": true}),
                    serde_json::json!({"ok": false, "compile_ok": true, "run_ok": false, "errors":["x"]}),
                ),
                // ok patch, but no-op (mutated=false)
                step(
                    "file",
                    serde_json::json!({"op":"patch","path":"models/a.sql","patch_text":"@@ ... @@\n- select 1\n+ select 1\n"}),
                    serde_json::json!({"ok": true, "mutated": false}),
                ),
            ],
            ..Default::default()
        };
        let g = derive_guard_state(Some(&log));
        assert!(g.last_validate_failed);
        assert!(!g.mutated_since_fail);
        assert!(g.patched_since_fail);
        match gate_authoring_to_validate(Some(&log)) {
            AuthoringGate::Allow => {}
            other => panic!("expected Allow, got {:?}", other),
        }
    }

    #[test]
    fn guard_clears_block_after_gold_model_mutation() {
        let log = ThreadLog {
            steps: vec![
                step(
                    "dbt_validate",
                    serde_json::json!({"build": true}),
                    serde_json::json!({"ok": false, "compile_ok": false, "run_ok": false, "errors":["x"]}),
                ),
                step(
                    "gold_model",
                    serde_json::json!({"items":[{"name":"fct_x"}]}),
                    serde_json::json!({"ok": true, "written_keys": ["k"]}),
                ),
            ],
            ..Default::default()
        };
        let g = derive_guard_state(Some(&log));
        assert!(g.last_validate_failed);
        assert!(g.mutated_since_fail);
    }

    #[test]
    fn guard_treats_dbt_validate_repairs_as_mutation_even_when_validate_failed() {
        let log = ThreadLog {
            steps: vec![step(
                "dbt_validate",
                serde_json::json!({"build": true}),
                serde_json::json!({
                    "ok": false,
                    "compile_ok": true,
                    "run_ok": false,
                    "errors": ["Runtime Error: x"],
                    "repair_report": {
                        "dialect": "Amazon Athena (engine v3 / Trino SQL)",
                        "max_iterations": 8,
                        "iterations_run": 1,
                        "iterations": [
                            {"iteration": 1, "llm_changed_files": 1}
                        ],
                        "stopped_reason": "llm_no_progress"
                    }
                }),
            )],
            ..Default::default()
        };
        let g = derive_guard_state(Some(&log));
        assert!(g.last_validate_failed);
        assert!(
            g.mutated_since_fail,
            "dbt_validate repairs should count as mutation to avoid deadlock"
        );
    }

    #[test]
    fn gate_blocks_after_three_failed_mutations() {
        let log = ThreadLog {
            steps: vec![
                step(
                    "dbt_validate",
                    serde_json::json!({"build": true}),
                    serde_json::json!({"ok": false, "compile_ok": false, "run_ok": false, "errors":["x"]}),
                ),
                step(
                    "staging_model",
                    serde_json::json!({"dataset_id":"AwsDataCatalog.test_raw.raw_customers"}),
                    serde_json::json!({"ok": false, "error": "tool timeout"}),
                ),
                step(
                    "staging_model",
                    serde_json::json!({"dataset_id":"AwsDataCatalog.test_raw.raw_customers"}),
                    serde_json::json!({"ok": false, "error": "tool timeout"}),
                ),
                step(
                    "file",
                    serde_json::json!({"op":"patch","path":"models/x.sql","patch_text":"@@ ... @@\n+select 1\n"}),
                    serde_json::json!({"ok": false, "error": "tool timeout"}),
                ),
            ],
            ..Default::default()
        };
        match gate_authoring_to_validate(Some(&log)) {
            AuthoringGate::Block { reason } => {
                assert!(reason.contains("failed 3 times"));
            }
            other => panic!("expected Block, got {:?}", other),
        }
    }

    #[test]
    fn gate_authoring_completion_allows_when_no_unresolved_mutation_failures() {
        let log = ThreadLog {
            steps: vec![
                step(
                    "phase",
                    serde_json::json!({"phase":"cleanse_author"}),
                    serde_json::json!({"ok":true}),
                ),
                step(
                    "staging_model",
                    serde_json::json!({"dataset_ids":["AwsDataCatalog.test_raw.raw_orders"]}),
                    serde_json::json!({"ok": true}),
                ),
            ],
            ..Default::default()
        };
        match gate_authoring_completion(Some(&log), Phase::CleanseAuthor) {
            AuthoringGate::Allow => {}
            other => panic!("expected Allow, got {:?}", other),
        }
    }

    #[test]
    fn gate_authoring_completion_blocks_when_last_mutation_failed_in_phase() {
        let log = ThreadLog {
            steps: vec![
                step(
                    "phase",
                    serde_json::json!({"phase":"model_author"}),
                    serde_json::json!({"ok":true}),
                ),
                step(
                    "staging_model",
                    serde_json::json!({"dataset_ids":["AwsDataCatalog.test_raw.raw_orders"]}),
                    serde_json::json!({"ok": false, "error":"bad sql"}),
                ),
            ],
            ..Default::default()
        };
        match gate_authoring_completion(Some(&log), Phase::ModelAuthor) {
            AuthoringGate::Block { reason } => {
                assert!(reason.contains("staging_model"));
                assert!(reason.contains("bad sql"));
            }
            other => panic!("expected Block, got {:?}", other),
        }
    }

    #[test]
    fn gate_authoring_completion_clears_failures_after_successful_mutation() {
        let log = ThreadLog {
            steps: vec![
                step(
                    "phase",
                    serde_json::json!({"phase":"model_author"}),
                    serde_json::json!({"ok":true}),
                ),
                step(
                    "staging_model",
                    serde_json::json!({"dataset_ids":["AwsDataCatalog.test_raw.raw_orders"]}),
                    serde_json::json!({"ok": false, "error":"timeout"}),
                ),
                step(
                    "staging_model",
                    serde_json::json!({"dataset_ids":["AwsDataCatalog.test_raw.raw_orders"]}),
                    serde_json::json!({"ok": true, "written_keys": ["k"]}),
                ),
            ],
            ..Default::default()
        };
        match gate_authoring_completion(Some(&log), Phase::ModelAuthor) {
            AuthoringGate::Allow => {}
            other => panic!("expected Allow, got {:?}", other),
        }
    }

    #[test]
    fn patch_failure_blocks_authoring_completion() {
        let log = ThreadLog {
            steps: vec![
                step(
                    "phase",
                    serde_json::json!({"phase":"cleanse_author"}),
                    serde_json::json!({"ok":true}),
                ),
                step(
                    "file",
                    serde_json::json!({"op":"patch","path":"models/x.sql","patch_text":"@@ ... @@\n- select 1\n+ select 1\n"}),
                    serde_json::json!({"ok": false, "errors":["invalid sql"]}),
                ),
            ],
            ..Default::default()
        };
        match gate_authoring_completion(Some(&log), Phase::CleanseAuthor) {
            AuthoringGate::Block { .. } => {}
            other => panic!("expected Block, got {:?}", other),
        }
    }

    #[test]
    fn guard_requires_probe_after_runtime_failure_and_accepts_probe_sql() {
        let log = ThreadLog {
            steps: vec![
                step(
                    "dbt_validate",
                    serde_json::json!({"build": true}),
                    serde_json::json!({
                        "ok": false,
                        "compile_ok": true,
                        "run_ok": false,
                        "runtime_failures": [{"name":"t","failures":1}],
                        "errors":["Runtime Error: test failed"]
                    }),
                ),
                step(
                    "run_sql",
                    serde_json::json!({"sql":"SELECT count(*) FROM AwsDataCatalog.db.t"}),
                    serde_json::json!({"ok": true, "rows":[["1"]]}),
                ),
            ],
            ..Default::default()
        };
        let g = derive_guard_state(Some(&log));
        assert!(
            !g.probe_required,
            "probe_required should be cleared once satisfied"
        );
        assert!(g.probe_satisfied);
    }

    #[tokio::test]
    async fn append_phase_with_reason_records_complete_reason_fields() {
        use react_core::keyspace::DefaultKeyspace;
        use react_core::scope::RequestScope;
        use react_core::storage::InMemoryStorageAdapter;
        use std::sync::Arc;

        let storage = Arc::new(InMemoryStorageAdapter::default());
        let scope = RequestScope {
            tenant: "t".into(),
            workspace: "w".into(),
            project_id: "p".into(),
        };
        let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
        let store = ThreadStore::new(storage, scope, keyspace);

        append_phase_with_reason(
            &store,
            "tid",
            Some("agent".to_string()),
            Some(Phase::Preflight),
            Phase::CleansePlan,
            Some(PhaseReasonCode::PreflightOk),
            Some(serde_json::json!({"x": 1, "nested": {"y": "z"}})),
        )
        .await
        .expect("append ok");

        let log = store.get("tid").await.expect("thread log should exist");
        let last = log.steps.last().expect("last step exists");
        let ThreadStep::Phase {
            phase,
            from_phase,
            reason_code,
            reason_detail,
            ..
        } = last
        else {
            panic!("expected Phase step");
        };
        assert_eq!(phase.as_str(), "cleanse_plan");
        assert_eq!(from_phase.as_deref(), Some("preflight"));
        assert_eq!(reason_code, &Some(PhaseReasonCode::PreflightOk));
        let rd = reason_detail.as_ref().expect("reason_detail should exist");
        assert_eq!(rd.get("x").and_then(|v| v.as_i64()), Some(1));
        assert_eq!(
            rd.get("nested")
                .and_then(|v| v.get("y"))
                .and_then(|v| v.as_str()),
            Some("z")
        );
    }

    #[tokio::test]
    async fn append_phase_with_reason_includes_null_fields_when_absent() {
        use react_core::keyspace::DefaultKeyspace;
        use react_core::scope::RequestScope;
        use react_core::storage::InMemoryStorageAdapter;
        use std::sync::Arc;

        let storage = Arc::new(InMemoryStorageAdapter::default());
        let scope = RequestScope {
            tenant: "t".into(),
            workspace: "w".into(),
            project_id: "p".into(),
        };
        let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
        let store = ThreadStore::new(storage, scope, keyspace);

        append_phase_with_reason(
            &store,
            "tid2",
            Some("agent".to_string()),
            None,
            Phase::CleanseAuthor,
            None,
            None,
        )
        .await
        .expect("append ok");
        let log = store.get("tid2").await.expect("thread log should exist");
        let last = log.steps.last().expect("last step exists");
        let ThreadStep::Phase {
            phase,
            from_phase,
            reason_code,
            reason_detail,
            ..
        } = last
        else {
            panic!("expected Phase step");
        };
        assert_eq!(phase.as_str(), "cleanse_author");
        assert!(from_phase.is_none());
        assert!(reason_code.is_none());
        assert!(reason_detail.is_none());
    }

    fn make_minimal_ctx(
        storage: std::sync::Arc<dyn react_core::storage::StorageAdapter>,
    ) -> AgentCtx {
        use react_core::agent::DefaultPolicy;
        use react_core::keyspace::DefaultKeyspace;
        use react_core::keyspace::Keyspace;
        use react_core::llm::NullModel;
        use react_core::scope::RequestScope;

        let keyspace: std::sync::Arc<dyn Keyspace> =
            std::sync::Arc::new(DefaultKeyspace::new("b".to_string()));
        let scope = RequestScope {
            tenant: "t".into(),
            workspace: "w".into(),
            project_id: "p".into(),
        };
        AgentCtx {
            top_k: 1,
            per_step_timeout_secs: 1,
            max_steps: 1,
            thread_id: None,
            progress_tx: None,
            pre_step_tx: None,
            trace_tx: None,
            agent_name: Some("test".to_string()),
            policy: std::sync::Arc::new(DefaultPolicy),
            llm: std::sync::Arc::new(NullModel {}),
            storage,
            scope,
            keyspace,
            query: None,
            warehouse: std::sync::Arc::new(react_core::providers::NullWarehouseProvider::default()),
            dbt: None,
            vector: None,
            thread_store: None,
            exec_ctx: None,
            runtime: None,
        }
    }

    #[tokio::test]
    async fn derive_targeted_select_terms_falls_back_to_path_selectors_when_manifest_missing() {
        use react_core::storage::InMemoryStorageAdapter;

        let storage: std::sync::Arc<dyn react_core::storage::StorageAdapter> =
            std::sync::Arc::new(InMemoryStorageAdapter::default());
        let ctx = make_minimal_ctx(storage);
        let log = ThreadLog {
            steps: vec![step(
                "file",
                serde_json::json!({"op":"patch"}),
                serde_json::json!({"ok": true, "results":[{"path":"models/staging/stg_a.sql","mutated":true}]}),
            )],
            ..Default::default()
        };
        let sel = derive_targeted_select_terms(&ctx, &log).await;
        assert_eq!(sel, vec!["+path:models/staging/stg_a.sql".to_string()]);
    }

    #[tokio::test]
    async fn derive_targeted_select_terms_prefers_manifest_model_names_when_available() {
        use react_core::storage::InMemoryStorageAdapter;

        let storage: std::sync::Arc<dyn react_core::storage::StorageAdapter> =
            std::sync::Arc::new(InMemoryStorageAdapter::default());
        let ctx = make_minimal_ctx(storage.clone());

        // Seed manifest.json under the standard dbt target key.
        let base = ctx
            .keyspace
            .dbt_prefix(&ctx.scope)
            .trim_end_matches('/')
            .to_string()
            + "/";
        let manifest_key = format!("{}target/manifest.json", base);
        let manifest = serde_json::json!({
            "nodes": {
                "model.data_engineer.stg_a": {
                    "resource_type": "model",
                    "name": "stg_a",
                    "original_file_path": "models/staging/stg_a.sql"
                }
            }
        });
        ctx.storage
            .put_bytes(
                &manifest_key,
                manifest.to_string().as_bytes(),
                "application/json",
            )
            .await
            .unwrap();

        let log = ThreadLog {
            steps: vec![step(
                "file",
                serde_json::json!({"op":"patch"}),
                serde_json::json!({"ok": true, "results":[{"path":"models/staging/stg_a.sql","mutated":true}]}),
            )],
            ..Default::default()
        };
        let sel = derive_targeted_select_terms(&ctx, &log).await;
        assert_eq!(sel, vec!["+stg_a".to_string()]);
    }

    #[tokio::test]
    async fn derive_targeted_select_terms_skips_when_global_impact_files_touched() {
        use react_core::storage::InMemoryStorageAdapter;

        let storage: std::sync::Arc<dyn react_core::storage::StorageAdapter> =
            std::sync::Arc::new(InMemoryStorageAdapter::default());
        let ctx = make_minimal_ctx(storage);
        let log = ThreadLog {
            steps: vec![step(
                "file",
                serde_json::json!({"op":"patch"}),
                serde_json::json!({"ok": true, "results":[{"path":"packages.yml","mutated":true},{"path":"models/staging/stg_a.sql","mutated":true}]}),
            )],
            ..Default::default()
        };
        let sel = derive_targeted_select_terms(&ctx, &log).await;
        assert!(sel.is_empty());
    }

    #[test]
    fn author_execution_gate_blocks_non_executable_cleanse_plan() {
        let plan = crate::data_engineer::plan::CleansePlan {
            plan_key: "k".to_string(),
            status: crate::data_engineer::plan::PlanStatus::Approved,
            project_snapshot: serde_json::json!({}),
            tasks: vec![crate::data_engineer::plan::CleanseTask {
                dataset_id: "a.b.c".to_string(),
                expected_model_path: Some("models/staging/stg_b_c.sql".to_string()),
                invariants: vec![],
                implementation_spec: crate::data_engineer::plan::CleanseImplementationSpec {
                    spec_version: 1,
                    row_preserving: true,
                    output_fields: vec![],
                    prohibited_ops: vec![],
                },
                status: crate::data_engineer::plan::TaskStatus::Pending,
                checklist: vec![],
            }],
            batches: vec![vec!["a.b.c".to_string()]],
            work_groups: vec![],
            mutations: vec![],
            progress: crate::data_engineer::plan::PlanProgress::default(),
        };
        match gate_author_phase_execution_cleanse(&plan) {
            AuthoringGate::Block { reason } => {
                assert!(reason.contains("not executable"));
                assert!(reason.contains("work_groups is empty"));
            }
            other => panic!("expected Block, got {:?}", other),
        }
    }

    #[test]
    fn author_execution_gate_allows_executable_model_plan() {
        let names = vec![vec!["fct_orders".to_string()]];
        let plan = crate::data_engineer::plan::ModelPlan {
            plan_key: "k".to_string(),
            status: crate::data_engineer::plan::PlanStatus::Approved,
            project_snapshot: serde_json::json!({}),
            tasks: vec![crate::data_engineer::plan::ModelTask {
                name: "fct_orders".to_string(),
                folder: "marts".to_string(),
                goal: "orders fact".to_string(),
                inputs: vec!["stg_orders".to_string()],
                expected_model_path: Some("models/marts/fct_orders.sql".to_string()),
                invariants: vec![],
                implementation_spec: crate::data_engineer::plan::ModelImplementationSpec {
                    spec_version: 1,
                    grain: "1 row per order".to_string(),
                    inputs: vec!["stg_orders".to_string()],
                    joins: vec![],
                    metrics: vec![],
                    output_fields: vec![],
                    assumptions: vec![],
                },
                status: crate::data_engineer::plan::TaskStatus::Pending,
                checklist: crate::data_engineer::plan::canonical_task_checklist(false),
            }],
            batches: names.clone(),
            work_groups: crate::data_engineer::plan::canonical_work_groups_from_batches(
                &names, "model",
            ),
            mutations: vec![],
            progress: crate::data_engineer::plan::PlanProgress::default(),
        };
        match gate_author_phase_execution_model(&plan) {
            AuthoringGate::Allow => {}
            other => panic!("expected Allow, got {:?}", other),
        }
    }
}
