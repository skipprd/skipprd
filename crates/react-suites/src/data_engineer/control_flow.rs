use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use tokio::time::timeout;
use tracing::warn;
 
use react_core::agent::AgentCtx;
use react_core::providers::DbtValidateArgs;
use react_core::session::{ThreadLog, ThreadStep, ThreadStore};
use react_core::tools::Tool;
 
use crate::config;
use crate::dbt;
use crate::data_engineer::tools::dbt_files::DbtFilesTool;
 
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Preflight,
    CleanseAuthor,
    CleanseValidate,
    CleanseReview,
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
            Phase::CleanseAuthor => "cleanse_author",
            Phase::CleanseValidate => "cleanse_validate",
            Phase::CleanseReview => "cleanse_review",
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
            "cleanse_author" => Some(Phase::CleanseAuthor),
            "cleanse_validate" => Some(Phase::CleanseValidate),
            "cleanse_review" => Some(Phase::CleanseReview),
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
    let Some(log) = log else { return Phase::Preflight };
    for step in log.steps.iter().rev() {
        if step.action != "phase" {
            continue;
        }
        let p = step
            .args
            .get("phase")
            .and_then(|v| v.as_str())
            .and_then(Phase::from_str);
        if let Some(p) = p {
            return p;
        }
    }
    Phase::Preflight
}
 
pub async fn append_phase(store: &ThreadStore, thread_id: &str, agent: Option<String>, phase: Phase) {
    // Best-effort: infer the previous phase if one exists; otherwise use null.
    let prev_phase = store
        .get(thread_id)
        .await
        .and_then(|log| {
            log.steps
                .iter()
                .rev()
                .find(|s| s.action == "phase")
                .and_then(|s| s.args.get("phase"))
                .and_then(|v| v.as_str())
                .and_then(Phase::from_str)
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
    .await;
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
) {
    let mut args = serde_json::Map::new();
    args.insert("phase".to_string(), serde_json::json!(phase.as_str()));
    // Always include these fields to keep thread JSON debuggable without inference.
    args.insert(
        "from_phase".to_string(),
        match from_phase {
            Some(fp) => serde_json::json!(fp.as_str()),
            None => Value::Null,
        },
    );
    args.insert(
        "reason_code".to_string(),
        match reason_code {
            Some(rc) => serde_json::json!(rc),
            None => Value::Null,
        },
    );
    args.insert(
        "reason_detail".to_string(),
        match reason_detail {
            Some(rd) => rd,
            None => Value::Null,
        },
    );
    let _ = store
        .append_step(
            thread_id,
            ThreadStep {
                action: "phase".to_string(),
                args: Value::Object(args),
                observation: serde_json::json!({ "ok": true }),
                ts: chrono::Utc::now().to_rfc3339(),
                agent,
            },
        )
        .await;
}
 
#[derive(Clone, Debug, Default)]
pub struct DerivedGuardState {
    pub last_validate_failed: bool,
    pub mutated_since_fail: bool,
    pub mutation_failures_since_validate: usize,
    pub probe_required: bool,
    pub probe_satisfied: bool,
}
 
fn is_mutation_step(step: &ThreadStep) -> bool {
    match step.action.as_str() {
        "approve_and_save_artifact" | "approve_and_save_artifact_batch" | "staging_model" => true,
        "dbt_files" => step
            .args
            .get("op")
            .and_then(|v| v.as_str())
            .map(|s| s == "put")
            .unwrap_or(false),
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
        if step.action == "dbt_validate" {
            last_validate_idx = Some(i);
            break;
        }
    }
    let Some(vidx) = last_validate_idx else { return out };
    let vstep = &log.steps[vidx];
 
    let ok = vstep.observation.get("ok").and_then(|x| x.as_bool()).unwrap_or(false);
    let compile_ok = vstep
        .observation
        .get("compile_ok")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    let run_ok = vstep.observation.get("run_ok").and_then(|x| x.as_bool());
    let build = vstep.args.get("build").and_then(|x| x.as_bool()).unwrap_or(false);
    let run = vstep.args.get("run").and_then(|x| x.as_bool()).unwrap_or(false);
    let runtime_validate = build || run;
 
    let ok_for_clear = if runtime_validate {
        ok && compile_ok && run_ok == Some(true)
    } else {
        ok && compile_ok
    };
    out.last_validate_failed = !ok_for_clear;
 
    // Probe requirement: compile ok but runtime failed with runtime_failures present.
    if runtime_validate && compile_ok && run_ok == Some(false) {
        let has_runtime_failures = vstep
            .observation
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
        let ok = step.observation.get("ok").and_then(|x| x.as_bool()).unwrap_or(false);
        if is_mutation_step(step) {
            if ok {
                out.mutated_since_fail = true;
            } else if !out.mutated_since_fail {
                // Count only consecutive failures until we see a successful mutation.
                out.mutation_failures_since_validate = out.mutation_failures_since_validate.saturating_add(1);
            }
        }
        if out.probe_required && ok && step.action == "run_sql" {
            let sql = step.args.get("sql").and_then(|x| x.as_str()).unwrap_or("");
            if looks_like_data_probe_sql(sql) {
                out.probe_satisfied = true;
            }
        }
    }
 
    // If probe has been satisfied, clear requirement (for gating).
    if out.probe_satisfied {
        out.probe_required = false;
    }
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
        if step.action != "phase" {
            continue;
        }
        let p = step
            .args
            .get("phase")
            .and_then(|v| v.as_str())
            .and_then(Phase::from_str);
        if p == Some(phase) {
            return Some(i);
        }
    }
    None
}

fn unresolved_mutation_failures_in_phase(log: &ThreadLog, phase: Phase) -> Vec<String> {
    // Collect failures since the last successful mutation in the current phase.
    // If a successful mutation occurs, it "clears" prior failures.
    let Some(start) = phase_start_idx(log, phase) else { return Vec::new() };

    let mut failures: Vec<String> = Vec::new();
    for step in log.steps.iter().skip(start + 1) {
        if !is_mutation_step(step) {
            continue;
        }
        let ok = step.observation.get("ok").and_then(|x| x.as_bool()).unwrap_or(false);
        if ok {
            failures.clear();
            continue;
        }
        let err = step
            .observation
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        failures.push(format!("{}: {}", step.action, err));
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
    if g.last_validate_failed && !g.mutated_since_fail {
        if g.mutation_failures_since_validate >= 3 {
            return AuthoringGate::AwaitUser {
                prompt: format!(
                    "I tried to apply a mutating fix after a failed dbt_validate, but the mutation step failed {} times in a row (often due to tool timeouts or storage write failures).\n\nPlease check:\n- The runtime can write DBT files to storage (S3 prefix/permissions)\n- The agent tool timeout is sufficient for your environment\n\nThen retry. If you want a quick deterministic fix path, use `dbt_files op=patch` to edit the failing model SQL directly.",
                    g.mutation_failures_since_validate
                ),
            };
        }
        return AuthoringGate::Block {
            reason: "A DBT validation previously failed and no successful mutation has been recorded since that failure. Apply a mutating fix (e.g. dbt_files op=patch / approve_and_save_artifact(_batch)) before re-validating."
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
    let Some(log) = log else { return AuthoringGate::Allow };
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
    AuthoringGate::Block { reason: msg.trim().to_string() }
}
 
pub struct DeterministicDbtValidateOnce;
 
impl DeterministicDbtValidateOnce {
    pub async fn run(
        ctx: &AgentCtx,
        build: bool,
        run: bool,
        dataset_ids: Option<&[String]>,
    ) -> Result<Value, String> {
        let dbt = ctx.dbt.as_ref().ok_or_else(|| "dbt provider missing".to_string())?;
        let Some(cfg) = config::resolved_config_from_ctx(ctx) else {
            return Err("resolved_config missing (needed to generate profiles.yml deterministically)".to_string());
        };
        let gen = dbt::profile::generate_profiles_yml(cfg)?;
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
                },
            )
            .await?;
 
        let mut v = serde_json::to_value(res)
            .unwrap_or_else(|_| serde_json::json!({"ok": false, "error": "failed to serialize result"}));
        if let Some(obj) = v.as_object_mut() {
            obj.insert(
                "dialect".to_string(),
                serde_json::json!(crate::data_engineer::dbt_repair::remediate::active_provider_dialect(cfg)),
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
 
pub async fn call_and_record_tool(
    store: &ThreadStore,
    thread_id: &str,
    agent: Option<String>,
    tool: &dyn Tool,
    args: Value,
    ctx: &AgentCtx,
    timeout_secs: u64,
) -> Value {
    let obs = match timeout(
        Duration::from_secs(timeout_secs.max(1)),
        tool.call(args.clone(), ctx),
    )
    .await
    {
        Ok(r) => r.unwrap_or_else(|e| serde_json::json!({"ok": false, "error": e})),
        Err(_) => serde_json::json!({"ok": false, "error": "tool timeout"}),
    };
    let _ = store
        .append_step(
            thread_id,
            ThreadStep {
                action: tool.name().to_string(),
                args,
                observation: obs.clone(),
                ts: chrono::Utc::now().to_rfc3339(),
                agent,
            },
        )
        .await;
    obs
}
 
/// Deterministic authoring invariant: ensure there is at least one model SQL file in `models/`.
pub async fn invariant_has_any_models(ctx: &AgentCtx) -> Result<bool, String> {
    let tool = DbtFilesTool { datasets: None };
    let obs = tool
        .call(serde_json::json!({"op":"list","prefix":"models/","limit":500}), ctx)
        .await
        .unwrap_or_else(|e| serde_json::json!({"ok": false, "error": e}));
    if obs.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        warn!("dbt_files list failed: {:?}", obs.get("error"));
        return Ok(false);
    }
    let items = obs.get("items").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let mut n_sql = 0usize;
    for it in items {
        let Some(p) = it.get("path").and_then(|v| v.as_str()) else { continue };
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
        .call(serde_json::json!({"op":"get","path":"dbt_project.yml","max_chars":2000}), ctx)
        .await
        .unwrap_or_else(|e| serde_json::json!({"ok": false, "error": e}));
    Ok(obs.get("ok").and_then(|v| v.as_bool()) == Some(true))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(action: &str, args: Value, observation: Value) -> ThreadStep {
        ThreadStep {
            action: action.to_string(),
            args,
            observation,
            ts: chrono::Utc::now().to_rfc3339(),
            agent: Some("test".to_string()),
        }
    }

    #[test]
    fn phase_is_derived_from_last_phase_step() {
        let log = ThreadLog {
            steps: vec![
                step("phase", serde_json::json!({"phase":"preflight"}), serde_json::json!({"ok":true})),
                step("phase", serde_json::json!({"phase":"model_author"}), serde_json::json!({"ok":true})),
            ],
            result: None,
            title: None,
            title_finalized: false,
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
            result: None,
            title: None,
            title_finalized: false,
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
                    serde_json::json!({"op":"put","path":"models/a.sql","content":"select 1"}),
                    serde_json::json!({"ok": true}),
                ),
            ],
            result: None,
            title: None,
            title_finalized: false,
        };
        let g = derive_guard_state(Some(&log));
        assert!(g.last_validate_failed);
        assert!(g.mutated_since_fail);
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
                    serde_json::json!({"op":"put","path":"models/x.sql","content":"select 1"}),
                    serde_json::json!({"ok": false, "error": "tool timeout"}),
                ),
            ],
            result: None,
            title: None,
            title_finalized: false,
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
                step("phase", serde_json::json!({"phase":"cleanse_author"}), serde_json::json!({"ok":true})),
                step(
                    "staging_model",
                    serde_json::json!({"dataset_ids":["AwsDataCatalog.test_raw.raw_orders"]}),
                    serde_json::json!({"ok": true}),
                ),
            ],
            result: None,
            title: None,
            title_finalized: false,
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
                step("phase", serde_json::json!({"phase":"model_author"}), serde_json::json!({"ok":true})),
                step(
                    "staging_model",
                    serde_json::json!({"dataset_ids":["AwsDataCatalog.test_raw.raw_orders"]}),
                    serde_json::json!({"ok": false, "error":"bad sql"}),
                ),
            ],
            result: None,
            title: None,
            title_finalized: false,
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
                step("phase", serde_json::json!({"phase":"model_author"}), serde_json::json!({"ok":true})),
                step(
                    "staging_model",
                    serde_json::json!({"dataset_ids":["AwsDataCatalog.test_raw.raw_orders"]}),
                    serde_json::json!({"ok": false, "error":"timeout"}),
                ),
                step(
                    "staging_model",
                    serde_json::json!({"dataset_ids":["AwsDataCatalog.test_raw.raw_orders"]}),
                    serde_json::json!({"ok": true}),
                ),
            ],
            result: None,
            title: None,
            title_finalized: false,
        };
        match gate_authoring_completion(Some(&log), Phase::ModelAuthor) {
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
            result: None,
            title: None,
            title_finalized: false,
        };
        let g = derive_guard_state(Some(&log));
        assert!(!g.probe_required, "probe_required should be cleared once satisfied");
        assert!(g.probe_satisfied);
    }

    #[tokio::test]
    async fn append_phase_with_reason_records_complete_reason_fields() {
        use react_core::keyspace::DefaultKeyspace;
        use react_core::scope::RequestScope;
        use react_core::storage::InMemoryStorageAdapter;
        use std::sync::Arc;

        let storage = Arc::new(InMemoryStorageAdapter::default());
        let scope = RequestScope { tenant: "t".into(), workspace: "w".into(), project_id: "p".into() };
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
        assert_eq!(last.action, "phase");
        assert_eq!(last.args.get("phase").and_then(|v| v.as_str()), Some("cleanse_author"));
        assert!(last.args.get("from_phase").is_some());
        assert!(last.args.get("reason_code").is_some());
        assert!(last.args.get("reason_detail").is_some());
        assert_eq!(last.args.get("from_phase").and_then(|v| v.as_str()), Some("preflight"));
        assert_eq!(last.args.get("reason_code").and_then(|v| v.as_str()), Some("preflight_ok"));
        assert_eq!(last.args.get("reason_detail").and_then(|v| v.get("x")).and_then(|v| v.as_i64()), Some(1));
        assert_eq!(
            last.args
                .get("reason_detail")
                .and_then(|v| v.get("nested"))
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
        let scope = RequestScope { tenant: "t".into(), workspace: "w".into(), project_id: "p".into() };
        let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
        let store = ThreadStore::new(storage, scope, keyspace);

        append_phase_with_reason(&store, "tid2", Some("agent".to_string()), None, Phase::CleanseAuthor, None, None).await;
        let log = store.get("tid2").await.expect("thread log should exist");
        let last = log.steps.last().expect("last step exists");
        assert_eq!(last.action, "phase");
        assert_eq!(last.args.get("phase").and_then(|v| v.as_str()), Some("cleanse_author"));
        assert!(last.args.get("from_phase").is_some());
        assert!(last.args.get("reason_code").is_some());
        assert!(last.args.get("reason_detail").is_some());
        assert!(last.args.get("from_phase").unwrap().is_null());
        assert!(last.args.get("reason_code").unwrap().is_null());
        assert!(last.args.get("reason_detail").unwrap().is_null());
    }
}
 
