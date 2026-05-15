use async_trait::async_trait;

use self::policy_sql_validated::DatasetCandidate;
use self::policy_sql_validated::SqlValidatedPolicy;
use self::preflight::PreflightProvider;
use crate::domain_types::GuardBlockKind;
use crate::phase_contract::commit_guard_block as apply_guard_block;
use react_core::agent::{
    Agent, AgentCtx, AgentPolicy, InterruptKind, RunOutcome, RunOutcomeNonInteractive,
};
use react_core::keyspace::encode_key_component;
use react_core::llm::LlmCallOptions;
use react_core::session::ThreadStore;
use react_core::suite::{FlowFrame, FlowKind, Suite, SuiteCtx};
use react_core::tools::ToolRegistry;
use std::collections::{BTreeSet, HashSet};

pub struct DataEngineerSuite;

pub(crate) fn resolved_config_from_ctx(
    ctx: &AgentCtx,
) -> Option<&react_core::resolved_config::ReactResolvedConfig> {
    ctx.resolved_config().as_ref().map(|c| c.as_ref())
}

impl react_core::suite::WorkflowSuiteContract for DataEngineerSuite {
    type Phase = control_flow::Phase;
    type ReasonCode = ();
    type GuardKind = GuardBlockKind;
    type State = crate::progress_controller::ExecutionState;
    type Event = crate::progress_controller::DataEngineerEvent;

    fn phase_as_str(phase: Self::Phase) -> &'static str {
        phase.as_str()
    }

    fn reason_as_str(_reason: Self::ReasonCode) -> &'static str {
        "unused"
    }

    fn guard_kind_as_str(kind: Self::GuardKind) -> &'static str {
        kind.as_str()
    }

    fn is_backtrack(from: Self::Phase, to: Self::Phase) -> bool {
        control_flow::is_replan_backtrack(from, to)
    }

    fn replan_backtrack_cap() -> usize {
        control_flow::replan_backtrack_counter_cap()
    }

    fn pre_turn(
        state: &crate::progress_controller::ExecutionState,
    ) -> react_core::workflow::PreTurnDirective<Self::GuardKind> {
        let phase = state.phase_state().current_phase;
        crate::phase_gate::evaluate_pre_turn_directive(
            state,
            phase,
            control_flow::replan_backtrack_counter_cap(),
        )
    }

    fn reduce(
        state: &mut crate::progress_controller::ExecutionState,
        event: crate::progress_controller::DataEngineerEvent,
    ) {
        state.apply_event(event);
    }
}

impl react_core::suite::WorkflowNodeContract for DataEngineerSuite {
    type Node = crate::control_flow::Phase;

    fn node_from_state(state: &crate::progress_controller::ExecutionState) -> Self::Node {
        state.phase.current_phase
    }

    fn phase_from_node(node: Self::Node) -> Self::Phase {
        node
    }
}

mod agent_modes;
pub(crate) mod authoring_contract;
pub(crate) mod authoring_driver;
pub(crate) mod authoring_ir;
mod catalog_bootstrap;
pub(crate) mod chunk_progress_contract;
pub(crate) mod control_flow;
pub(crate) mod controller_event;
pub(crate) mod controller_kernel;
pub mod ctx_ext;
pub(crate) mod dataset_truth;
pub(crate) mod dbt;
pub(crate) mod dbt_error;
pub mod de_config;
pub mod debug;
pub(crate) mod dialect;
pub(crate) mod domain_types;
mod enrichment;
pub(crate) mod enrichment_concurrency;
pub(crate) mod env_util;
pub(crate) mod facts;
pub mod failure_kind;
pub mod failure_text;
pub mod file_ownership;
mod llm_profiles;
pub mod metering;
pub(crate) mod model_dispatch;
pub(crate) mod naming;
pub(crate) mod patch_contract;
pub(crate) mod patch_protocol;
pub(crate) mod patch_schemas;
mod phase_author;
mod phase_author_lifecycle;
pub(crate) mod phase_contract;
mod phase_el_discover;
mod phase_el_sync;
mod phase_el_verify;
pub(crate) mod phase_gate;
mod phase_plan;
mod phase_plan_lifecycle;
mod phase_preflight;
mod phase_publish;
pub(crate) mod phase_reason_detail;
mod phase_review;
mod phase_validate;
pub(crate) mod plan;
pub(crate) mod plan_diff;
mod plan_grounding;
pub(crate) mod plan_kind;
pub(crate) mod plan_progress;
mod plan_review_helpers;
pub(crate) mod plan_schema;
mod plan_storage;
mod plan_types;
mod plan_validation;
pub(crate) mod policy_sql_validated;
pub(crate) mod preflight;
pub(crate) mod probe_target;
pub(crate) mod progress_controller;
pub(crate) mod project_fs;
pub mod prompt_packets;
pub(crate) mod prompts;
pub mod providers;
pub(crate) mod references;
pub(crate) mod repair_session;
pub(crate) mod repair_subroutine;
pub(crate) mod retry_budget;
mod review_batched;
mod review_persistence;
mod review_prompts;
pub(crate) mod schema_policy;
mod semantic_profile;
pub(crate) mod sql_first;
pub(crate) mod state_manager;
pub(crate) mod thread_cache;
pub(crate) mod tool_ops;
mod tool_policies;
mod tool_registry_builder;
pub mod tools;
mod track_spec;
pub(crate) mod transient_retry;
pub(crate) mod transition_dispatcher;
pub(crate) mod vector_docs;
pub(crate) mod ws_plans;
use agent_modes::{AgentMode, AgentToolCapability};
pub use ctx_ext::copy_capabilities_to_actx;
use llm_profiles::PlanningLlmProfile;
pub use plan_types::{StrippedArtifact, MAX_STRIPPED_ARTIFACTS};
pub(crate) use track_spec::TrackKind;

/// dbt `profiles.yml` bundle for Skippr CLI / IDE (`skippr test`), same rendering as `dbt_validate`.
pub use crate::dbt::profile::GeneratedProfiles;

/// Generate `profiles.yml` for Skippr CLI / IDE (`skippr test`), mirroring `dbt_validate`.
pub fn skippr_cli_generate_dbt_profiles_yml(
    cfg: &react_core::resolved_config::ReactResolvedConfig,
    threads: Option<usize>,
) -> Result<GeneratedProfiles, String> {
    crate::dbt::profile::generate_profiles_yml(cfg, threads)
}

pub(crate) use react_core::workflow::PhaseOutcome;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct DataEngineerThreadStatus {
    pub current_phase: String,
    pub is_done: bool,
    pub has_failure_context: bool,
    pub failure_brief: Option<String>,
    pub repair_status: String,
    pub pending_plan_revision: bool,
}

pub async fn load_thread_status(
    control: &react_core::session::ControlStateStore,
    thread_id: &str,
) -> Result<Option<DataEngineerThreadStatus>, String> {
    let Some(state) = state_manager::load_execution_state_strict(control, thread_id)
        .await
        .map_err(|e| e.to_string())?
    else {
        return Ok(None);
    };
    let current_phase = state.phase.current_phase;
    let failure_brief = state
        .repair
        .failure_context
        .as_ref()
        .map(|ctx| ctx.brief.clone());
    Ok(Some(DataEngineerThreadStatus {
        current_phase: current_phase.as_str().to_string(),
        is_done: current_phase == control_flow::Phase::Done,
        has_failure_context: failure_brief.is_some(),
        failure_brief,
        repair_status: match &state.repair.status {
            progress_controller::RepairStatus::Idle => "idle".to_string(),
            progress_controller::RepairStatus::Pending { cycle } => {
                format!("pending(cycle={cycle})")
            }
            progress_controller::RepairStatus::Exhausted { cycles_used } => {
                format!("exhausted(cycles_used={cycles_used})")
            }
        },
        pending_plan_revision: state.phase.pending_plan_revision.is_some(),
    }))
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PhaseError {
    #[error(transparent)]
    State(#[from] state_manager::StateError),
    #[error(transparent)]
    Plan(#[from] plan_storage::PlanError),
    #[error(transparent)]
    Transition(#[from] transition_dispatcher::TransitionError),
    #[error("tool contract violation: {0}")]
    ToolContractViolation(String),
    #[error("{0}")]
    Fatal(String),
}

impl From<String> for PhaseError {
    fn from(s: String) -> Self {
        PhaseError::Fatal(s)
    }
}

/// Interrupt policy used by non-deterministic single-pass modes.
///
/// Deterministic agent-mode orchestration enforces its own non-interactive contract.
struct InterruptOnlyPolicy;

#[async_trait::async_trait]
impl AgentPolicy for InterruptOnlyPolicy {
    fn interrupt_for_action(
        &self,
        action_name: &str,
        args: &serde_json::Value,
        obs: &serde_json::Value,
    ) -> Option<(InterruptKind, String)> {
        if action_name == "ask_user" {
            let prompt = args
                .get("prompt")
                .and_then(|x| x.as_str())
                .or_else(|| obs.get("prompt").and_then(|x| x.as_str()))
                .unwrap_or("Please provide additional context.")
                .to_string();
            return Some((InterruptKind::AwaitUser, prompt));
        }
        if action_name == "ask_approval" {
            let prompt = args
                .get("prompt")
                .and_then(|x| x.as_str())
                .or_else(|| obs.get("prompt").and_then(|x| x.as_str()))
                .unwrap_or("Please review and approve/reject.")
                .to_string();
            return Some((InterruptKind::AwaitApproval, prompt));
        }
        None
    }

    fn clean_tool_name(&self, name: &str, args: &serde_json::Value) -> String {
        crate::control_flow::de_clean_tool_name(name, args)
    }

    fn timeout_for_tool(&self, action_name: &str) -> Option<u64> {
        use crate::env_util::{
            TOOL_TIMEOUT_EXTRA_SLOW_SECS, TOOL_TIMEOUT_FAST_SECS, TOOL_TIMEOUT_MEDIUM_SECS,
            TOOL_TIMEOUT_SLOW_SECS,
        };
        let secs = match action_name {
            "staging_model" | "apply_next_cleanse_batch" | "apply_next_cleanse_schema_batch" => {
                TOOL_TIMEOUT_SLOW_SECS
            }

            "gold_model" | "apply_next_model_batch" | "apply_next_model_schema_batch" => {
                TOOL_TIMEOUT_MEDIUM_SECS
            }

            "dbt_validate" | "publish_dbt_to_provider" => TOOL_TIMEOUT_EXTRA_SLOW_SECS,

            "preflight_catalog_all" => TOOL_TIMEOUT_SLOW_SECS,
            "preflight_catalog_dataset" | "preflight_catalog_schema" => TOOL_TIMEOUT_MEDIUM_SECS,

            _ => TOOL_TIMEOUT_FAST_SECS,
        };
        Some(secs)
    }

    async fn handle_complete(
        &self,
        tools: &ToolRegistry,
        ctx: &AgentCtx,
        transcript: &mut Vec<String>,
        store: Option<&react_core::session::ThreadStore>,
        thread_id: &str,
        complete_env: &react_core::agent::CompleteEnvelope,
    ) -> Result<react_core::agent::CompleteDecision, String> {
        if complete_env.kind == "blocking_requirement" {
            return Ok(react_core::agent::CompleteDecision::Reject {
                reason: "blocking_requirement is not a valid completion. \
                         You have tool access to read files and apply patches. \
                         Use the available tools to read any files you need and \
                         then apply the fix in this same turn."
                    .to_string(),
            });
        }
        react_core::agent::DefaultPolicy
            .handle_complete(tools, ctx, transcript, store, thread_id, complete_env)
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

// TODO(item-93): PlanState encodes track + mode in variant names. Consider restructuring as
// a struct with `track: TrackKind` + `mode: AuthoringMode` fields, with `AuthoringMode` being
// an enum { Sql(Vec<String>), Schema(Vec<String>), Unconstrained }. This would eliminate the
// combinatorial explosion as new tracks are added. Deferred due to widespread pattern matching.
#[derive(Clone, Debug)]
enum PlanState {
    CleanseSqlDatasetIds(Vec<String>),
    CleanseSchemaDatasetIds(Vec<String>),
    ModelSqlItemNames(Vec<String>),
    Unconstrained,
    /// Post-validation-failure or review-patch: only surgical file/SQL tools,
    /// no bulk authoring or batch tools.
    Repair,
    /// Read-only planning phases: discovery tools only, no mutations.
    ReadOnly,
}

#[derive(Clone, Debug)]
struct NonEmptyCleanseDatasetIds {
    ids: Vec<String>,
}

impl NonEmptyCleanseDatasetIds {
    fn from_discovered_raw(discovered_raw: &BTreeSet<String>) -> Result<Self, String> {
        let ids: Vec<String> = discovered_raw
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if ids.is_empty() {
            return Err(
                "cleanse planning has no discovered raw datasets; cannot build deterministic task skeleton"
                    .to_string(),
            );
        }
        Ok(Self { ids })
    }

    fn as_slice(&self) -> &[String] {
        &self.ids
    }
}

impl DataEngineerSuite {
    fn first_column_name_from_sql_schema_observation(obs: &serde_json::Value) -> Option<String> {
        obs.get("columns")
            .and_then(|v| v.as_array())
            .and_then(|arr| {
                arr.iter().find_map(|c| {
                    c.get("name")
                        .and_then(|x| x.as_str())
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                })
            })
    }

    fn synthesize_cleanse_plan_from_grounded_raw(
        plan: &mut crate::plan::CleansePlan,
        allowed_raw: &BTreeSet<String>,
    ) -> bool {
        if allowed_raw.is_empty() {
            return false;
        }
        let ids: Vec<String> = allowed_raw.iter().cloned().collect();
        let tasks = ids
            .iter()
            .map(|dataset_id| crate::plan::CleanseTask {
                dataset_id: dataset_id.clone(),
                expected_model_path: None,
                invariants: vec![],
                implementation_spec: Some(crate::plan::CleanseImplementationSpec {
                    spec_version: 1,
                    row_preserving: true,
                    output_fields: vec![],
                    prohibited_ops: vec![],
                }),
                source_schema: vec![],
                status: crate::plan::TaskStatus::Pending,
                checklist: crate::plan::canonical_task_checklist(TrackKind::Cleanse),
            })
            .collect::<Vec<_>>();
        let batches = ids
            .chunks(plan_progress::MAX_BATCH_SIZE)
            .map(|c| c.to_vec())
            .collect::<Vec<_>>();
        plan.tasks = tasks;
        plan.batches = batches;
        plan.work_groups =
            crate::plan::canonical_work_groups_from_batches(&plan.batches, "cleanse");
        true
    }

    fn collect_cleanse_grounding_candidates(
        plan: &crate::plan::CleansePlan,
        discovered_raw: &BTreeSet<String>,
    ) -> Vec<String> {
        let mut out: BTreeSet<String> = BTreeSet::new();
        for t in plan.tasks.iter() {
            let id = t.dataset_id.trim();
            if !id.is_empty() {
                out.insert(id.to_string());
            }
        }
        for b in plan.batches.iter() {
            for ds in b.iter() {
                let id = ds.trim();
                if !id.is_empty() {
                    out.insert(id.to_string());
                }
            }
        }
        // Deterministic safety rail: when the skeleton parser yields no task ids,
        // seed grounding from observed raw relations in this plan phase.
        if out.is_empty() {
            out.extend(discovered_raw.iter().cloned());
        }
        out.into_iter().collect()
    }

    fn deterministic_cleanse_skeleton_from_discovered_raw(
        discovered_raw: &BTreeSet<String>,
    ) -> Result<crate::plan_schema::CleansePlanSkeletonV1, String> {
        let ids = NonEmptyCleanseDatasetIds::from_discovered_raw(discovered_raw)?;
        let tasks = ids
            .as_slice()
            .iter()
            .map(|dataset_id| crate::plan_schema::CleansePlanSkeletonTaskV1 {
                dataset_id: dataset_id.clone(),
            })
            .collect::<Vec<_>>();
        let batches = ids
            .as_slice()
            .chunks(plan_progress::MAX_BATCH_SIZE)
            .map(|chunk| chunk.to_vec())
            .collect::<Vec<_>>();
        Ok(crate::plan_schema::CleansePlanSkeletonV1 { tasks, batches })
    }

    async fn run_deterministic_probe_for_table(
        thread_store: &ThreadStore,
        thread_id: &str,
        actx: &AgentCtx,
        sql_schema_tool: &tools::sql_schema::SqlSchemaTool,
        sql_stats_tool: &tools::sql_stats::SqlStatsTool,
        _sql_sample_tool: &tools::sql_sample::SqlSampleTool,
        run_sql_tool: &tools::sql_run::SqlRunTool,
        table: &str,
        sql_schema_timeout: u64,
        sql_stats_timeout: u64,
        _sql_sample_timeout: u64,
        run_sql_timeout: u64,
    ) -> (bool, Option<String>) {
        let schema_obs = control_flow::call_and_record_tool(
            thread_store,
            thread_id,
            Some(env_util::DEFAULT_AGENT_NAME.to_string()),
            sql_schema_tool,
            serde_json::json!({"table": table}),
            actx,
            sql_schema_timeout,
        )
        .await;
        let first_field = Self::first_column_name_from_sql_schema_observation(&schema_obs);
        if let Some(field) = first_field.as_ref() {
            let stats_obs = control_flow::call_and_record_tool(
                thread_store,
                thread_id,
                Some(env_util::DEFAULT_AGENT_NAME.to_string()),
                sql_stats_tool,
                serde_json::json!({"table": table, "field": field}),
                actx,
                sql_stats_timeout,
            )
            .await;
            if stats_obs
                .get("ok")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                return (true, Some(field.clone()));
            }
        }
        let run_obs = control_flow::call_and_record_tool(
            thread_store,
            thread_id,
            Some(env_util::DEFAULT_AGENT_NAME.to_string()),
            run_sql_tool,
            serde_json::json!({"sql": format!("SELECT count(*) AS total FROM {}", table)}),
            actx,
            run_sql_timeout,
        )
        .await;
        (
            run_obs.get("ok").and_then(|v| v.as_bool()).unwrap_or(false),
            first_field,
        )
    }

    fn churn_audit_acceptance_criteria() -> serde_json::Value {
        serde_json::json!({
            "no_false_completion": "plans are never marked completed while checklist items remain pending",
            "no_non_executable_authoring": "authoring exits to planning when executable work-group structure is invalid",
            "no_review_churn_from_missing_staging": "model plan grounding loops must not be caused by skipped upstream authoring",
        })
    }

    fn excerpt(s: &str, max_chars: usize) -> String {
        if s.chars().count() <= max_chars {
            return s.to_string();
        }
        let mut out = s.chars().take(max_chars).collect::<String>();
        out.push_str("\n... (truncated)");
        out
    }

    fn parse_json_object_strict(raw: &str) -> Result<serde_json::Value, String> {
        serde_json::from_str::<serde_json::Value>(raw).map_err(|e| {
            let preview: String = raw.chars().take(500).collect();
            tracing::error!(
                raw_len = raw.len(),
                raw_preview = %preview,
                "parse_json_object_strict failed: {e}"
            );
            e.to_string()
        })
    }

    fn parse_json_typed_strict<T: serde::de::DeserializeOwned>(raw: &str) -> Result<T, String> {
        let v = Self::parse_json_object_strict(raw)?;
        serde_json::from_value::<T>(v).map_err(|e| e.to_string())
    }

    fn sanitize_impl_spec_value(
        mut v: serde_json::Value,
        track: TrackKind,
    ) -> (serde_json::Value, Vec<String>) {
        let mut stripped: Vec<String> = Vec::new();
        let Some(obj) = v.as_object_mut() else {
            return (v, stripped);
        };
        let allowed: HashSet<&'static str> = if track == TrackKind::Cleanse {
            [
                "spec_version",
                "row_preserving",
                "output_fields",
                "prohibited_ops",
            ]
            .into_iter()
            .collect()
        } else {
            [
                "spec_version",
                "grain",
                "inputs",
                "joins",
                "metrics",
                "output_fields",
                "assumptions",
                "evidence_claim_refs",
            ]
            .into_iter()
            .collect()
        };
        let keys: Vec<String> = obj.keys().cloned().collect();
        for k in keys {
            if !allowed.contains(k.as_str()) {
                obj.remove(&k);
                stripped.push(k);
            }
        }
        (v, stripped)
    }

    fn parse_impl_spec_value_with_sanitize<T: serde::de::DeserializeOwned>(
        v: serde_json::Value,
        track: TrackKind,
    ) -> Result<(T, Vec<String>), String> {
        let (sv, stripped) = Self::sanitize_impl_spec_value(v, track);
        Self::validate_output_field_kind_contract(&sv)?;
        let spec = serde_json::from_value::<T>(sv).map_err(|e| e.to_string())?;
        Ok((spec, stripped))
    }

    fn validate_output_field_kind_contract(v: &serde_json::Value) -> Result<(), String> {
        let Some(obj) = v.as_object() else {
            return Ok(());
        };
        let Some(output_fields) = obj.get("output_fields") else {
            return Ok(());
        };
        let Some(arr) = output_fields.as_array() else {
            return Ok(());
        };
        let mut errs: Vec<String> = Vec::new();
        for (idx, it) in arr.iter().enumerate() {
            let Some(kv) = it.get("kind") else {
                continue;
            };
            let Some(ks) = kv.as_str() else {
                errs.push(format!("output_fields[{idx}].kind must be a string"));
                continue;
            };
            if !matches!(ks, "raw" | "clean" | "derived" | "quality_flag") {
                errs.push(format!("output_fields[{idx}].kind='{}' is invalid", ks));
            }
        }
        if errs.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "{}; allowed kind values: raw, clean, derived, quality_flag",
                errs.join("; ")
            ))
        }
    }

    fn push_snapshot_array_event(
        snapshot: &mut crate::plan_types::PlanSnapshot,
        key: &str,
        event: serde_json::Value,
        max_len: usize,
    ) {
        let arr = snapshot
            .extra
            .entry(key.to_string())
            .or_insert_with(|| serde_json::Value::Array(vec![]));
        if let Some(items) = arr.as_array_mut() {
            items.push(event);
            while items.len() > max_len {
                items.remove(0);
            }
        }
    }
}

#[async_trait]
impl Suite for DataEngineerSuite {
    fn id(&self) -> &'static str {
        "data_engineer"
    }

    fn label(&self) -> &'static str {
        "Data Engineer"
    }

    fn supported_agent_types(&self) -> Vec<String> {
        vec!["ask".to_string(), "agent".to_string(), "review".to_string()]
    }

    fn default_agent_type(&self) -> &'static str {
        "ask"
    }

    fn phase_order(&self, agent_type: &str) -> Vec<String> {
        // Only expose phases for agent-mode; other modes are single-pass.
        if AgentMode::parse(agent_type).ok() != Some(AgentMode::Agent) {
            return Vec::new();
        }
        use crate::control_flow::Phase;
        vec![
            Phase::ElDiscover.as_str(),
            Phase::ElSync.as_str(),
            Phase::ElVerify.as_str(),
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
            Phase::Done.as_str(),
        ]
        .into_iter()
        .map(|s| s.to_string())
        .collect()
    }

    async fn load_ws_plans(
        &self,
        thread_id: &str,
        ctx: &SuiteCtx,
    ) -> Result<Vec<serde_json::Value>, String> {
        Ok(ws_plans::load_latest_plans_ws(ctx, thread_id).await)
    }

    async fn handle_new(
        &self,
        thread_id: &str,
        question: &str,
        agent_type: &str,
        ctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String> {
        let _ = ctx
            .log_writer()
            .ensure_preflight_phase_step(
                thread_id,
                agent_type,
                Some(self.id()),
                self.initial_phase(),
            )
            .await;
        let frames = self
            .dispatch_agent(thread_id, question, agent_type, ctx)
            .await?;
        ctx.record_flow_frames(thread_id, agent_type, &frames).await;
        Ok(frames)
    }

    async fn handle_open(
        &self,
        thread_id: &str,
        question: &str,
        agent_type: &str,
        ctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String> {
        let _ = ctx
            .log_writer()
            .ensure_preflight_phase_step(
                thread_id,
                agent_type,
                Some(self.id()),
                self.initial_phase(),
            )
            .await;
        let frames = self
            .dispatch_agent(thread_id, question, agent_type, ctx)
            .await?;
        ctx.record_flow_frames(thread_id, agent_type, &frames).await;
        Ok(frames)
    }

    async fn handle_user(
        &self,
        thread_id: &str,
        text: &str,
        agent_type: &str,
        ctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String> {
        let _ = ctx
            .log_writer()
            .ensure_preflight_phase_step(
                thread_id,
                agent_type,
                Some(self.id()),
                self.initial_phase(),
            )
            .await;
        let frames = self
            .dispatch_agent(thread_id, text, agent_type, ctx)
            .await?;
        ctx.record_flow_frames(thread_id, agent_type, &frames).await;
        Ok(frames)
    }
}

#[cfg(test)]
#[path = "tests_mod.rs"]
mod tests;
