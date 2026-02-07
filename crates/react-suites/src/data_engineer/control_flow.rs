use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use tokio::time::timeout;
use tracing::warn;

use react_core::agent::AgentCtx;
use react_core::providers::DbtValidateArgs;
use react_core::session::{Observation, ThreadLog, ThreadStep, ThreadStore, ToolObservation};
use react_core::tools::Tool;

use crate::config;
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
        Some("phase_set"),
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
    reason_code: Option<&str>,
    reason_detail: Option<Value>,
) -> Result<(), String> {
    let agent = agent.unwrap_or_else(|| "unknown".to_string());
    store
        .append_step(
            thread_id,
            ThreadStep::Phase {
                phase: phase.as_str().to_string(),
                from_phase: from_phase.map(|p| p.as_str().to_string()),
                reason_code: reason_code.map(|s| s.to_string()),
                reason_detail,
                observation: Observation::ok(),
                ts: chrono::Utc::now().to_rfc3339(),
                agent,
            },
        )
        .await
}

#[derive(Clone, Debug, Default)]
pub struct DerivedGuardState {
    pub last_validate_failed: bool,
    pub mutated_since_fail: bool,
    /// True if at least one successful dbt_files op=patch occurred since the failing validate,
    /// even if it ended up being a no-op write (mutated=false).
    ///
    /// This is used as a conservative "we did try to apply a fix" signal to avoid deadlocking
    /// the suite purely due to mutation detection brittleness.
    pub patched_since_fail: bool,
    pub mutation_failures_since_validate: usize,
    pub probe_required: bool,
    pub probe_satisfied: bool,
}

fn is_mutation_step(step: &ThreadStep) -> bool {
    match step {
        ThreadStep::ToolEnd { name, args, .. } => match name.as_str() {
            "approve_and_save_artifact"
            | "approve_and_save_artifact_batch"
            | "staging_model"
            | "gold_model" => true,
            "dbt_files" => {
                // preview_diff is explicitly non-mutating (no write occurs), so it should not
                // be treated as a mutation attempt for any guard logic.
                let preview = args
                    .get("preview_diff")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(false);
                if preview {
                    return false;
                }
                args.get("op")
                    .and_then(|v| v.as_str())
                    .map(|s| s == "patch")
                    .unwrap_or(false)
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

    let (name, args, observation) = match step {
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
        "dbt_files" => {
            // preview_diff means no write occurred; do not treat as a mutation.
            let preview = args
                .get("preview_diff")
                .and_then(|x| x.as_bool())
                .unwrap_or(false);
            if preview {
                return false;
            }
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
                // Successful dbt_files patch counts as "patch applied", even if no-op.
                if let ThreadStep::ToolEnd {
                    name,
                    args,
                    observation,
                    ..
                } = step
                {
                    if name == "dbt_files" && observation.ok {
                        let preview = args
                            .get("preview_diff")
                            .and_then(|x| x.as_bool())
                            .unwrap_or(false);
                        let is_patch = args
                            .get("op")
                            .and_then(|v| v.as_str())
                            .map(|s| s == "patch")
                            .unwrap_or(false);
                        if is_patch && !preview {
                            out.patched_since_fail = true;
                        }
                    }
                }
                if is_effective_mutation_step(step) {
                    out.mutated_since_fail = true;
                }
                // ok-but-ineffective (preview/no-op) is intentionally NOT treated as a mutation or a failure.
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
/// successful `dbt_files op=patch` (non-preview) step in the thread.
///
/// Strategy:
/// - Prefer manifest-based mapping from patched file path -> model name (when manifest is available)
/// - Fall back to dbt path selectors: `path:<rel_path>`
/// - If the patch touched global-impact files (macros/, packages.yml, dbt_project.yml), return an
///   empty list to indicate we should skip targeted validation and do full validation instead.
pub async fn derive_targeted_select_terms(ctx: &AgentCtx, log: &ThreadLog) -> Vec<String> {
    // Find the most recent successful dbt_files patch (non-preview).
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
        if name != "dbt_files" || !observation.ok {
            continue;
        }
        let preview = args
            .get("preview_diff")
            .and_then(|x| x.as_bool())
            .unwrap_or(false);
        if preview {
            continue;
        }
        let is_patch = args
            .get("op")
            .and_then(|v| v.as_str())
            .map(|s| s == "patch")
            .unwrap_or(false);
        if !is_patch {
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
            if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
                let p = p.trim();
                if !p.is_empty() {
                    patched_paths.push(p.to_string());
                }
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

#[derive(Clone, Debug)]
pub enum AuthoringGate {
    Allow,
    Block { reason: String },
    AwaitUser { prompt: String },
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
            .errors
            .first()
            .map(|s| s.as_str())
            .unwrap_or("unknown error");
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
            return AuthoringGate::AwaitUser {
                prompt: format!(
                    "I tried to apply a mutating fix after a failed dbt_validate, but the mutation step failed {} times in a row (often due to tool timeouts or storage write failures).\n\nPlease check:\n- The runtime can write DBT files to storage (S3 prefix/permissions)\n- The agent tool timeout is sufficient for your environment\n\nThen retry. If you want a quick deterministic fix path, use `dbt_files op=patch` to edit the failing model SQL directly.",
                    g.mutation_failures_since_validate
                ),
            };
        }
        return AuthoringGate::Block {
            reason: "A DBT validation previously failed and no successful mutation has been recorded since that failure. Apply a mutating fix (e.g. dbt_files op=patch or staging_model) before re-validating."
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
            "dbt_files" => {
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
                    "get_json" => {
                        let p = args
                            .get("path")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .trim();
                        if !p.is_empty() {
                            return format!("Read JSON {p}");
                        }
                        "Read JSON".to_string()
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
                            return format!("dbt_files {op}");
                        }
                        "dbt_files".to_string()
                    }
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
    let _ = store
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
        .await;

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
    let _ = store
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
        .await;
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
        warn!("dbt_files list failed: {:?}", obs.get("error"));
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
                    .map(|s| s.to_string()),
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
                    "dbt_files",
                    serde_json::json!({"op":"patch","path":"models/a.sql","content":"select 1"}),
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
                    "dbt_files",
                    serde_json::json!({"op":"patch","replace_file":{"path":"models/a.sql","new_text":"select 1\n"}}),
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
    fn gate_awaits_user_after_three_failed_mutations() {
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
                    "dbt_files",
                    serde_json::json!({"op":"patch","path":"models/x.sql","content":"select 1"}),
                    serde_json::json!({"ok": false, "error": "tool timeout"}),
                ),
            ],
            ..Default::default()
        };
        match gate_authoring_to_validate(Some(&log)) {
            AuthoringGate::AwaitUser { prompt } => {
                assert!(prompt.contains("failed 3 times"));
            }
            other => panic!("expected AwaitUser, got {:?}", other),
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
    fn preview_patch_failure_does_not_block_authoring_completion() {
        let log = ThreadLog {
            steps: vec![
                step(
                    "phase",
                    serde_json::json!({"phase":"cleanse_author"}),
                    serde_json::json!({"ok":true}),
                ),
                step(
                    "dbt_files",
                    serde_json::json!({"op":"patch","preview_diff": true, "replace_file": {"path":"models/x.sql","new_text":"select 1\n"}}),
                    serde_json::json!({"ok": false, "errors":["invalid sql"]}),
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
            Phase::CleanseAuthor,
            Some("preflight_ok"),
            Some(serde_json::json!({"x": 1, "nested": {"y": "z"}})),
        )
        .await;

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
        assert_eq!(phase.as_str(), "cleanse_author");
        assert_eq!(from_phase.as_deref(), Some("preflight"));
        assert_eq!(reason_code.as_deref(), Some("preflight_ok"));
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
        .await;
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
                "dbt_files",
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
                "dbt_files",
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
                "dbt_files",
                serde_json::json!({"op":"patch"}),
                serde_json::json!({"ok": true, "results":[{"path":"packages.yml","mutated":true},{"path":"models/staging/stg_a.sql","mutated":true}]}),
            )],
            ..Default::default()
        };
        let sel = derive_targeted_select_terms(&ctx, &log).await;
        assert!(sel.is_empty());
    }
}
