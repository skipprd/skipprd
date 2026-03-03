use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use tokio::time::timeout;
use tracing::warn;

use react_core::agent::AgentCtx;
use react_core::control_flow::PhaseReasonCode;
use react_core::providers::DbtValidateArgs;
use react_core::session::{ThreadStep, ThreadStore, ToolObservation, ToolStepStatus};
use react_core::tools::Tool;

use crate::config;
use crate::data_engineer::tools::files_tool::FilesTool;
use crate::dbt;
pub use react_core::workflow::TransitionIntent;

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

#[cfg(test)]
mod state_first_tests {
    use super::*;

    #[test]
    fn derive_guard_state_from_execution_state_marks_validate_failure() {
        let mut st = crate::data_engineer::progress_controller::ExecutionState::new();
        st.telemetry.last_validate = Some(crate::data_engineer::progress_controller::LastValidateState {
            ok: Some(false),
            ..crate::data_engineer::progress_controller::LastValidateState::default()
        });
        let guard = derive_guard_state_from_execution_state(&st);
        assert!(guard.last_validate_failed);
        assert!(!guard.mutated_since_fail);
    }

    #[test]
    fn derive_guard_state_from_execution_state_tracks_mutation_progress() {
        let mut st = crate::data_engineer::progress_controller::ExecutionState::new();
        st.telemetry.last_validate = Some(crate::data_engineer::progress_controller::LastValidateState {
            ok: Some(false),
            ..crate::data_engineer::progress_controller::LastValidateState::default()
        });
        st.repair.last_progress_delta = Some(crate::data_engineer::progress_controller::ProgressDelta {
            target_hash_changed: true,
            failed_target_count_delta: 0,
            failure_signature_changed: false,
            checklist_completed_delta: 0,
            progress_made: true,
        });
        let guard = derive_guard_state_from_execution_state(&st);
        assert!(guard.last_validate_failed);
        assert!(guard.mutated_since_fail);
    }
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

pub(crate) fn allowed_next_phases(from: Phase) -> &'static [Phase] {
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

pub(crate) fn is_annotation_reason(reason_code: Option<PhaseReasonCode>) -> bool {
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

pub(crate) fn replan_backtrack_counter_cap() -> usize {
    std::env::var("AGENT_MAX_REPLAN_BACKTRACKS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(3)
        .max(2)
        .min(20)
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

pub fn derive_guard_state_from_execution_state(
    st: &crate::data_engineer::progress_controller::ExecutionState,
) -> DerivedGuardState {
    let last_validate_failed = st
        .telemetry
        .last_validate
        .as_ref()
        .and_then(|lv| lv.ok)
        == Some(false);
    let mutated_since_fail = st
        .repair
        .last_progress_delta
        .as_ref()
        .map(|d| d.target_hash_changed || d.progress_made)
        .unwrap_or(false);
    let patched_since_fail = st.attempt_count() > 0;
    let probe_status = st.probe_requirement_status();
    let (probe_required, probe_satisfied) = match probe_status {
        crate::data_engineer::progress_controller::ProbeRequirementStatus::NotRequired => {
            (false, true)
        }
        crate::data_engineer::progress_controller::ProbeRequirementStatus::Required => {
            (true, false)
        }
        crate::data_engineer::progress_controller::ProbeRequirementStatus::Allowed => {
            (true, true)
        }
        crate::data_engineer::progress_controller::ProbeRequirementStatus::ExhaustedRequireMutation => {
            (false, true)
        }
    };
    DerivedGuardState {
        last_validate_failed,
        mutated_since_fail,
        patched_since_fail,
        mutation_failures_since_validate: st.consecutive_noop_patches(),
        probe_required,
        probe_satisfied,
    }
}

pub(crate) fn is_cleanse_replan_backtrack(from: Phase, to: Phase) -> bool {
    matches!(from, Phase::CleanseValidate | Phase::CleanseReview)
        && matches!(to, Phase::CleansePlan | Phase::CleanseAuthor)
}

pub(crate) fn is_model_replan_backtrack(from: Phase, to: Phase) -> bool {
    matches!(
        from,
        Phase::ModelValidate | Phase::ModelReview | Phase::PostPublishReview
    ) && matches!(to, Phase::ModelPlan | Phase::ModelAuthor)
}

#[derive(Clone, Debug)]
pub enum AuthoringGate {
    Allow,
    Block { reason: String },
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
    ) -> Result<crate::data_engineer::controller_event::ValidateObservationContract, String> {
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
        crate::data_engineer::controller_event::validate_contract_from_observation(v)
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
    ) -> Result<crate::data_engineer::controller_event::ValidateObservationContract, String> {
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
        crate::data_engineer::controller_event::validate_contract_from_observation(v)
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
                status: ToolStepStatus::Running,
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
    if tool.name() == "json_file" {
        let manifest_path = args
            .get("path")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("");
        if let Some(path_kind) =
            crate::data_engineer::progress_controller::classify_manifest_lookup_path(manifest_path)
        {
            match crate::data_engineer::state_manager::load_execution_state_strict(store, thread_id)
                .await
            {
                Ok(Some(mut st)) => {
                    if st.phase.current_phase == Some(Phase::ModelPlan) {
                        let failure_kind = if obs.ok {
                            None
                        } else {
                            crate::data_engineer::progress_controller::classify_manifest_lookup_failure(
                                &obs.errors,
                            )
                        };
                        st.note_manifest_lookup_attempt(path_kind, obs.ok, failure_kind);
                        if let Err(e) =
                            crate::data_engineer::state_manager::replace_execution_state(store, thread_id, st).await
                        {
                            warn!("failed to persist manifest lookup telemetry: {}", e);
                        }
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    warn!(
                        "failed to load strict execution state for manifest telemetry: {}",
                        e
                    );
                }
            }
        }
    }
    if let Some((op, affected_paths)) = (|| {
        let name = tool.name();
        match name {
            "apply_next_cleanse_batch" | "apply_next_cleanse_schema_batch" => {
                let succeeded: Vec<String> = obs
                    .extra
                    .get("succeeded_dataset_ids")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
                            .filter(|s| !s.is_empty())
                            .collect()
                    })
                    .unwrap_or_default();
                if succeeded.is_empty() {
                    None
                } else {
                    Some((name.to_string(), succeeded))
                }
            }
            "apply_next_model_batch" | "apply_next_model_schema_batch" => {
                let succeeded: Vec<String> = obs
                    .extra
                    .get("succeeded_item_names")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
                            .filter(|s| !s.is_empty())
                            .collect()
                    })
                    .unwrap_or_default();
                if succeeded.is_empty() {
                    None
                } else {
                    Some((name.to_string(), succeeded))
                }
            }
            "staging_model" | "gold_model" => {
                let written: Vec<String> = obs
                    .extra
                    .get("written_keys")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
                            .filter(|s| !s.is_empty())
                            .collect()
                    })
                    .unwrap_or_default();
                if written.is_empty() {
                    None
                } else {
                    Some((name.to_string(), written))
                }
            }
            _ => None,
        }
    })() {
        match crate::data_engineer::state_manager::load_execution_state_strict(store, thread_id)
            .await
        {
            Ok(Some(mut st)) => {
                st.set_last_mutation_summary(&op, affected_paths, Vec::new());
                if let Err(e) =
                    crate::data_engineer::state_manager::replace_execution_state(store, thread_id, st)
                        .await
                {
                    warn!("failed to persist non-file mutation summary: {}", e);
                }
            }
            Ok(None) => {}
            Err(e) => {
                warn!(
                    "failed to load strict execution state for mutation summary: {}",
                    e
                );
            }
        }
    }
    let status = if obs.ok {
        ToolStepStatus::Ok
    } else {
        ToolStepStatus::Failed
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
    let tool = FilesTool { datasets: None };
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
    let tool = FilesTool { datasets: None };
    let obs = tool
        .call(
            serde_json::json!({"op":"get","path":"dbt_project.yml","max_chars":2000}),
            ctx,
        )
        .await
        .unwrap_or_else(|e| serde_json::json!({"ok": false, "error": e}));
    Ok(obs.get("ok").and_then(|v| v.as_bool()) == Some(true))
}

#[cfg(all(test, any()))]
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
            "validate_pass_to_review" => Some(PhaseReasonCode::ValidatePassToReview),
            "validate_pass_to_authoring" => Some(PhaseReasonCode::ValidatePassToAuthoring),
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
                        react_core::session::ToolStepStatus::Ok
                    } else {
                        react_core::session::ToolStepStatus::Failed
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
    fn guard_does_not_treat_file_get_as_mutation() {
        let log = ThreadLog {
            steps: vec![
                step(
                    "dbt_validate",
                    serde_json::json!({"build": true}),
                    serde_json::json!({"ok": false, "compile_ok": false, "run_ok": false, "errors":["x"]}),
                ),
                step(
                    "file",
                    serde_json::json!({"op":"get","path":"models/a.sql"}),
                    serde_json::json!({"ok": true, "path":"models/a.sql","text":"select 1"}),
                ),
            ],
            ..Default::default()
        };
        let g = derive_guard_state(Some(&log));
        assert!(g.last_validate_failed);
        assert!(!g.mutated_since_fail);
        assert!(!g.patched_since_fail);
        match gate_authoring_to_validate(Some(&log)) {
            AuthoringGate::Block { .. } => {}
            other => panic!("expected Block, got {:?}", other),
        }
    }

    #[test]
    fn guard_does_not_treat_file_list_as_mutation() {
        let log = ThreadLog {
            steps: vec![
                step(
                    "dbt_validate",
                    serde_json::json!({"build": true}),
                    serde_json::json!({"ok": false, "compile_ok": false, "run_ok": false, "errors":["x"]}),
                ),
                step(
                    "file",
                    serde_json::json!({"op":"list","prefix":"models/","limit":10}),
                    serde_json::json!({"ok": true, "items":[{"path":"models/a.sql"}]}),
                ),
            ],
            ..Default::default()
        };
        let g = derive_guard_state(Some(&log));
        assert!(g.last_validate_failed);
        assert!(!g.mutated_since_fail);
        assert!(!g.patched_since_fail);
        match gate_authoring_to_validate(Some(&log)) {
            AuthoringGate::Block { .. } => {}
            other => panic!("expected Block, got {:?}", other),
        }
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

    #[test]
    fn execution_state_guard_uses_probe_state_required_and_exhausted() {
        let mut st = crate::data_engineer::progress_controller::ExecutionState::new();
        st.telemetry.last_validate = Some(crate::data_engineer::progress_controller::LastValidateState {
            ok: Some(false),
            ..crate::data_engineer::progress_controller::LastValidateState::default()
        });
        st.telemetry.probe.required = true;
        let g = derive_guard_state_from_execution_state(&st);
        assert!(g.probe_required);
        assert!(!g.probe_satisfied);

        let sig = crate::data_engineer::progress_controller::ProbeSignature::from_run_sql(
            "select * from x limit 10",
            &serde_json::json!({"ok": true}),
        );
        let _ = st.note_probe_attempt("select * from x limit 10", true, sig.clone());
        let _ = st.note_probe_attempt("select * from x limit 10", true, sig.clone());
        let _ = st.note_probe_attempt("select * from x limit 10", true, sig.clone());
        let _ = st.note_probe_attempt("select * from x limit 10", true, sig);

        let g2 = derive_guard_state_from_execution_state(&st);
        assert!(
            !g2.probe_required && g2.probe_satisfied,
            "exhausted probe loops should not keep requesting more probes"
        );
    }

    #[tokio::test]
    async fn append_phase_with_intent_records_complete_reason_fields() {
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

        crate::data_engineer::transition_dispatcher::apply_phase_directive(
            &store,
            "tid",
            Some("agent".to_string()),
            Some(Phase::Preflight),
            crate::data_engineer::transition_dispatcher::PhaseDirective::Transition {
                to: Phase::CleansePlan,
                intent: TransitionIntent::Forward,
                reason_code: Some(PhaseReasonCode::PreflightOk),
                reason_detail: Some(serde_json::json!({"x": 1, "nested": {"y": "z"}})),
            },
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
    async fn append_phase_with_intent_includes_null_fields_when_absent() {
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

        crate::data_engineer::transition_dispatcher::apply_phase_directive(
            &store,
            "tid2",
            Some("agent".to_string()),
            None,
            crate::data_engineer::transition_dispatcher::PhaseDirective::Transition {
                to: Phase::CleanseAuthor,
                intent: TransitionIntent::Forward,
                reason_code: None,
                reason_detail: None,
            },
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
            resolved_config: None,
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
