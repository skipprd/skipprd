use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::time::timeout;
use tracing::warn;

use crate::providers::DbtValidateArgs;
use react_core::agent::AgentCtx;
use react_core::session::{ThreadStore, ToolObservation, ToolStepMeta};
use react_core::tools::Tool;

use crate::dbt;
use crate::tools::files_tool::FilesTool;
pub use react_core::workflow::TransitionIntent;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    ElDiscover,
    ElSync,
    ElVerify,
    #[default]
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
    Done,
}

#[cfg(test)]
mod state_first_tests {
    use super::*;

    #[test]
    fn phase_as_str_from_str_round_trip() {
        for phase in ALL_PHASES {
            let s = phase.as_str();
            let back = Phase::from_str(s).unwrap_or_else(|| panic!("from_str failed for {s}"));
            assert_eq!(back, phase, "round-trip failed for {s}");
        }
    }

    #[test]
    fn phase_ordinal_is_sequential() {
        for (i, phase) in ALL_PHASES.iter().enumerate() {
            assert_eq!(phase.ordinal(), i, "ordinal mismatch for {:?}", phase);
        }
    }

    #[test]
    fn phase_serde_round_trip() {
        for phase in ALL_PHASES {
            let json = serde_json::to_string(&phase).unwrap();
            let back: Phase = serde_json::from_str(&json).unwrap();
            assert_eq!(back, phase, "serde round-trip failed for {:?}", phase);
            assert_eq!(
                json.trim_matches('"'),
                phase.as_str(),
                "serde name does not match as_str for {:?}",
                phase,
            );
        }
    }

    #[test]
    fn classify_telemetry_json_file_manifest() {
        let args = serde_json::json!({"path": "target/manifest.json"});
        let obs = react_core::session::ToolObservation::normalize(serde_json::json!({"ok": true}));
        let actions = classify_tool_telemetry("json_file", &args, &obs);
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            &actions[0],
            ToolTelemetryAction::ManifestLookup { ok: true, .. }
        ));
    }

    #[test]
    fn classify_telemetry_batch_tool_mutation() {
        let args = serde_json::json!({});
        let mut extra = std::collections::BTreeMap::new();
        extra.insert(
            "succeeded_dataset_ids".to_string(),
            serde_json::json!(["ds1", "ds2"]),
        );
        let obs = react_core::session::ToolObservation {
            ok: true,
            errors: vec![],
            warnings: vec![],
            extra,
        };
        let actions = classify_tool_telemetry("apply_next_cleanse_batch", &args, &obs);
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            &actions[0],
            ToolTelemetryAction::MutationRecord { paths, .. } if paths.len() == 2
        ));
    }

    #[test]
    fn classify_telemetry_unrecognized_tool_empty() {
        let args = serde_json::json!({});
        let obs = react_core::session::ToolObservation::normalize(serde_json::json!({"ok": true}));
        let actions = classify_tool_telemetry("unknown_tool", &args, &obs);
        assert!(actions.is_empty());
    }
}

pub const ALL_PHASES: [Phase; 15] = [
    Phase::ElDiscover,
    Phase::ElSync,
    Phase::ElVerify,
    Phase::Preflight,
    Phase::CleansePlan,
    Phase::CleanseAuthor,
    Phase::CleanseValidate,
    Phase::CleanseReview,
    Phase::ModelPlan,
    Phase::ModelAuthor,
    Phase::ModelValidate,
    Phase::ModelReview,
    Phase::PublishAwaitApproval,
    Phase::Publish,
    Phase::Done,
];

impl Phase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Phase::ElDiscover => "el_discover",
            Phase::ElSync => "el_sync",
            Phase::ElVerify => "el_verify",
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
            Phase::Done => "done",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        s.parse().ok()
    }

    pub fn ordinal(&self) -> usize {
        ALL_PHASES.iter().position(|p| p == self).unwrap_or(0)
    }

    pub fn tier(&self) -> crate::progress_controller::ExecutionTier {
        use crate::progress_controller::ExecutionTier;
        match self {
            Phase::ElDiscover | Phase::ElSync | Phase::ElVerify => ExecutionTier::El,
            Phase::CleansePlan
            | Phase::CleanseAuthor
            | Phase::CleanseValidate
            | Phase::CleanseReview => ExecutionTier::Cleanse,
            Phase::ModelPlan | Phase::ModelAuthor | Phase::ModelValidate | Phase::ModelReview => {
                ExecutionTier::Model
            }
            _ => ExecutionTier::Unknown,
        }
    }
}

impl std::str::FromStr for Phase {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "el_discover" => Ok(Phase::ElDiscover),
            "el_sync" => Ok(Phase::ElSync),
            "el_verify" => Ok(Phase::ElVerify),
            "preflight" => Ok(Phase::Preflight),
            "cleanse_plan" => Ok(Phase::CleansePlan),
            "cleanse_author" => Ok(Phase::CleanseAuthor),
            "cleanse_validate" => Ok(Phase::CleanseValidate),
            "cleanse_review" => Ok(Phase::CleanseReview),
            "model_plan" => Ok(Phase::ModelPlan),
            "model_author" => Ok(Phase::ModelAuthor),
            "model_validate" => Ok(Phase::ModelValidate),
            "model_review" => Ok(Phase::ModelReview),
            "publish_await_approval" => Ok(Phase::PublishAwaitApproval),
            "publish" => Ok(Phase::Publish),
            "done" => Ok(Phase::Done),
            _ => Err(format!("unknown phase: '{}'", s)),
        }
    }
}

pub(crate) fn allowed_next_phases(from: Phase) -> &'static [Phase] {
    match from {
        Phase::ElDiscover => &[Phase::ElSync],
        Phase::ElSync => &[Phase::ElSync, Phase::ElVerify],
        Phase::ElVerify => &[Phase::ElVerify, Phase::Preflight, Phase::ElSync],
        Phase::Preflight => &[Phase::CleansePlan],
        Phase::CleansePlan => &[Phase::CleansePlan, Phase::CleanseAuthor],
        Phase::CleanseAuthor => &[
            Phase::CleanseAuthor,
            Phase::CleanseValidate,
            Phase::CleansePlan,
        ],
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
        Phase::PublishAwaitApproval => &[
            Phase::PublishAwaitApproval,
            Phase::Publish,
            Phase::ModelReview,
        ],
        Phase::Publish => &[
            Phase::Publish,
            Phase::Done,
            Phase::ModelReview,
            Phase::ModelAuthor,
        ],
        Phase::Done => &[Phase::Done],
    }
}

pub(crate) fn replan_backtrack_counter_cap() -> usize {
    crate::env_util::max_replan_backtracks()
}

pub(crate) fn is_replan_backtrack(from: Phase, to: Phase) -> bool {
    use crate::track_spec::TrackKind;
    let from_track = TrackKind::from_any_phase(from);
    let Some(from_track) = from_track else {
        return false;
    };
    let Some(to_track) = TrackKind::from_any_phase(to) else {
        return false;
    };
    if from_track != to_track {
        return false;
    }
    let plan = from_track.plan_phase();
    let author = from_track.author_phase();
    from != plan && (to == plan || to == author)
}

#[derive(Clone, Debug)]
pub enum AuthoringGate {
    Allow,
    Block { reason: String },
}

pub fn gate_author_phase_execution(plan: &impl crate::plan_types::TrackPlan) -> AuthoringGate {
    let issues = plan.executable_plan_issues();
    if issues.is_empty() {
        return AuthoringGate::Allow;
    }
    let mut msg =
        String::from("Approved plan is not executable. Re-enter planning before authoring:\n");
    for issue in issues.iter().take(8) {
        msg.push_str("- ");
        msg.push_str(issue);
        msg.push('\n');
    }
    AuthoringGate::Block {
        reason: msg.trim().to_string(),
    }
}

/// Unified deterministic dbt validation. When `select` is `Some`, runs targeted
/// validation for a specific set of models; otherwise validates the whole project.
pub(crate) struct DeterministicDbtValidateOnce;

impl DeterministicDbtValidateOnce {
    pub async fn run(
        ctx: &AgentCtx,
        build: bool,
        run: bool,
        dataset_ids: Option<&[String]>,
    ) -> Result<crate::controller_event::ValidateObservationContract, String> {
        Self::run_inner(ctx, build, run, None, dataset_ids).await
    }

    async fn run_inner(
        ctx: &AgentCtx,
        build: bool,
        run: bool,
        select: Option<&[String]>,
        dataset_ids: Option<&[String]>,
    ) -> Result<crate::controller_event::ValidateObservationContract, String> {
        let dbt =
            crate::ctx_ext::actx_dbt(ctx).ok_or_else(|| "dbt provider missing".to_string())?;
        let Some(cfg) = crate::resolved_config_from_ctx(ctx) else {
            return Err(
                "resolved_config missing (needed to generate profiles.yml deterministically)"
                    .to_string(),
            );
        };
        let threads = crate::ctx_ext::actx_query(ctx)
            .as_ref()
            .map(|q| q.max_concurrency());
        let gen = dbt::profile::generate_profiles_yml(cfg, threads)?;
        let td = tempfile::tempdir().map_err(|e| e.to_string())?;
        let profiles_dir = td.path().to_string_lossy().to_string();
        let profiles_path = td.path().join("profiles.yml");
        std::fs::write(&profiles_path, gen.profiles_yml.as_bytes()).map_err(|e| e.to_string())?;

        let validate_args = DbtValidateArgs {
            project_name: crate::env_util::SUITE_PROJECT_NAME.to_string(),
            profiles_dir: Some(profiles_dir),
            target: gen.target,
            run,
            build,
            select: select.map(|s| s.to_vec()),
            exclude: None,
            tier_routing: Some(gen.tier_routing),
        };
        let res = crate::transient_retry::retry_transient_default(
            "deterministic_dbt_validate",
            || async { dbt.validate_project(ctx.scope(), &validate_args).await },
        )
        .await?;

        // Persist any strip-and-notify artifacts the sanitizer emitted during this dbt
        // invocation. See `crate::file_ownership` for the ownership policy that drives strips.
        if !res.stripped.is_empty() {
            crate::plan_storage::persist_stripped_artifacts(ctx, res.stripped.clone()).await;
        }

        let mut v = serde_json::to_value(res).unwrap_or_else(
            |_| serde_json::json!({"ok": false, "error": "failed to serialize result"}),
        );
        if let Some(obj) = v.as_object_mut() {
            obj.insert(
                "dialect".to_string(),
                serde_json::json!(crate::dialect::active_provider_dialect(cfg)),
            );
            if let Some(sel) = select {
                obj.insert("select".to_string(), serde_json::json!(sel));
            }
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
                if let Ok(sum) =
                    crate::dbt_error::summarize_dbt_failure_llm(ctx, &errors, &logs, 2000).await
                {
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
        crate::controller_event::validate_contract_from_observation(v)
    }
}

fn extract_string_vec_from_extra(extra: &BTreeMap<String, Value>, key: &str) -> Vec<String> {
    extra
        .get(key)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn de_clean_tool_name(name: &str, args: &Value) -> String {
    fn arg_str<'a>(args: &'a Value, key: &str) -> &'a str {
        args.get(key).and_then(|v| v.as_str()).unwrap_or("").trim()
    }

    struct ToolLabel {
        verb: &'static str,
        arg_key: &'static str,
        fallback: &'static str,
    }

    const SIMPLE_TOOLS: &[(&str, ToolLabel)] = &[
        (
            "json_file",
            ToolLabel {
                verb: "JSON",
                arg_key: "path",
                fallback: "JSON file",
            },
        ),
        (
            "sql_schema",
            ToolLabel {
                verb: "Describe",
                arg_key: "table",
                fallback: "List tables",
            },
        ),
        (
            "sql_stats",
            ToolLabel {
                verb: "Stats",
                arg_key: "table",
                fallback: "Stats",
            },
        ),
        (
            "sql_sample",
            ToolLabel {
                verb: "Sample",
                arg_key: "table",
                fallback: "Sample",
            },
        ),
    ];

    if name == "file" {
        let op = arg_str(args, "op");
        let key = if op == "list" { "prefix" } else { "path" };
        let verb = match op {
            "get" => "Read",
            "list" => "List",
            "patch" => "Patch",
            "write" => "Write",
            "rm" => "Remove",
            "mv" => "Move",
            other if !other.is_empty() => return format!("file {other}"),
            _ => return "file".to_string(),
        };
        let p = arg_str(args, key);
        return if p.is_empty() {
            format!("{verb} file")
        } else {
            format!("{verb} {p}")
        };
    }

    if name == "run_sql" {
        return "Run SQL".to_string();
    }

    for (tool, label) in SIMPLE_TOOLS {
        if name == *tool {
            let v = arg_str(args, label.arg_key);
            return if v.is_empty() {
                label.fallback.to_string()
            } else {
                format!("{} {v}", label.verb)
            };
        }
    }

    name.replace('_', " ")
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
    let agent_str = agent.unwrap_or_else(|| crate::env_util::UNKNOWN_AGENT.to_string());
    let tool_name = tool.name().to_string();
    let meta = ToolStepMeta {
        agent: agent_str,
        phase: "suite_tool_dispatch".to_string(),
        name: tool_name.clone(),
        clean_name: de_clean_tool_name(tool.name(), &args),
        args: args.clone(),
        ctx: ctx.exec_ctx().clone(),
    };

    store
        .run_observed(
            thread_id,
            meta,
            || async {
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
                record_post_tool_telemetry(store, thread_id, &tool_name, &args, &obs).await;
                Ok(raw)
            },
            |raw: &Value| Ok(raw.clone()),
        )
        .await
        .unwrap_or_else(|e| {
            warn!(tool = %tool_name, error = %e, "run_observed failed for tool dispatch");
            serde_json::json!({"ok": false, "errors": [e]})
        })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ToolTelemetryAction {
    ManifestLookup {
        path_kind: crate::progress_controller::ManifestLookupPathKind,
        ok: bool,
        failure_kind: Option<crate::progress_controller::ManifestLookupFailureKind>,
    },
    MutationRecord {
        op: crate::progress_controller::MutationOp,
        paths: Vec<String>,
    },
    ProbeAttemptRecord {
        sql: String,
        ok: bool,
        signature: crate::progress_controller::ProbeSignature,
    },
}

pub(crate) fn classify_tool_telemetry(
    tool_name: &str,
    args: &Value,
    obs: &ToolObservation,
) -> Vec<ToolTelemetryAction> {
    let mut actions = Vec::new();

    if tool_name == "json_file" {
        let manifest_path = args
            .get("path")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("");
        if let Some(path_kind) =
            crate::progress_controller::classify_manifest_lookup_path(manifest_path)
        {
            let failure_kind = if obs.ok {
                None
            } else {
                crate::progress_controller::classify_manifest_lookup_failure(&obs.errors)
            };
            actions.push(ToolTelemetryAction::ManifestLookup {
                path_kind,
                ok: obs.ok,
                failure_kind,
            });
        }
    }

    let key = match tool_name {
        "apply_next_cleanse_batch" | "apply_next_cleanse_schema_batch" => {
            Some("succeeded_dataset_ids")
        }
        "apply_next_model_batch" | "apply_next_model_schema_batch" => Some("succeeded_item_names"),
        "staging_model" | "gold_model" => Some("written_keys"),
        _ => None,
    };
    if let Some(key) = key {
        let items = extract_string_vec_from_extra(&obs.extra, key);
        if !items.is_empty() {
            actions.push(ToolTelemetryAction::MutationRecord {
                op: crate::progress_controller::MutationOp::Patch,
                paths: items,
            });
        }
    }

    if tool_name == "run_sql" {
        let sql = args
            .get("sql")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("");
        if !sql.is_empty() {
            let observation = Value::Object(
                obs.extra
                    .clone()
                    .into_iter()
                    .collect::<serde_json::Map<String, Value>>(),
            );
            actions.push(ToolTelemetryAction::ProbeAttemptRecord {
                sql: sql.to_string(),
                ok: obs.ok,
                signature: crate::progress_controller::ProbeSignature::from_run_sql(
                    sql,
                    &observation,
                ),
            });
        }
    }

    actions
}

async fn apply_telemetry_action(store: &ThreadStore, thread_id: &str, action: ToolTelemetryAction) {
    match action {
        ToolTelemetryAction::ManifestLookup {
            path_kind,
            ok,
            failure_kind,
        } => {
            let phase_ok = match crate::state_manager::load_execution_state_strict(
                &store.control_store(),
                thread_id,
            )
            .await
            {
                Ok(Some(st)) => st.phase.current_phase == Phase::ModelPlan,
                _ => false,
            };
            if phase_ok {
                if let Err(e) = crate::state_manager::apply_execution_event(
                    &store.control_store(),
                    thread_id,
                    crate::progress_controller::DataEngineerEvent::ManifestLookupRecorded {
                        path_kind,
                        success: ok,
                        failure_kind,
                    },
                )
                .await
                {
                    warn!("failed to persist manifest lookup telemetry: {e}");
                }
            }
        }
        ToolTelemetryAction::MutationRecord { op, paths } => {
            if let Err(e) = crate::state_manager::apply_execution_event(
                &store.control_store(),
                thread_id,
                crate::progress_controller::DataEngineerEvent::MutationRecorded {
                    op,
                    paths,
                    select_terms: Vec::new(),
                },
            )
            .await
            {
                warn!("failed to persist non-file mutation summary: {e}");
            }
        }
        ToolTelemetryAction::ProbeAttemptRecord { sql, ok, signature } => {
            if let Err(e) = crate::state_manager::apply_execution_event(
                &store.control_store(),
                thread_id,
                crate::progress_controller::DataEngineerEvent::ProbeAttemptRecorded {
                    sql,
                    ok,
                    signature,
                },
            )
            .await
            {
                warn!("failed to persist probe telemetry: {e}");
            }
        }
    }
}

async fn record_post_tool_telemetry(
    store: &ThreadStore,
    thread_id: &str,
    tool_name: &str,
    args: &Value,
    obs: &ToolObservation,
) {
    for action in classify_tool_telemetry(tool_name, args, obs) {
        apply_telemetry_action(store, thread_id, action).await;
    }
}

/// Deterministic authoring invariant: ensure there is at least one model SQL file in `models/`.
pub async fn invariant_has_any_models(ctx: &AgentCtx) -> Result<bool, String> {
    let tool = FilesTool { datasets: None };
    let obs = tool
        .call(
            serde_json::json!({"op":"list","prefix":"models/","limit":crate::env_util::FILE_LIST_LIMIT}),
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
            serde_json::json!({"op":"get","path":"dbt_project.yml","max_chars":crate::env_util::FILE_GET_MAX_CHARS}),
            ctx,
        )
        .await
        .unwrap_or_else(|e| serde_json::json!({"ok": false, "error": e}));
    Ok(obs.get("ok").and_then(|v| v.as_bool()) == Some(true))
}
