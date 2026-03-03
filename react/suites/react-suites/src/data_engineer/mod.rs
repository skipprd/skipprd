use async_trait::async_trait;
use serde::Serialize;
use serde_json::json;
use crate::data_engineer::authoring_driver::AuthoringKind;

use crate::data_engineer_shared::policy_sql_validated::SqlValidatedPolicy;
use crate::data_engineer_shared::types::DatasetCandidate;
use crate::flow_frame::FlowFrame;
use crate::preflight::PreflightProvider;
use crate::suite::{Suite, SuiteCtx};
use react_core::agent::{
    Agent, AgentCtx, AgentPolicy, Interrupt, RunOutcome, RunOutcomeNonInteractive,
};
use react_core::control_flow::{GuardBlockKind, PhaseReasonCode, ReviewDecision};
use react_core::llm::LlmCallOptions;
use react_core::session::{CatalogBootstrapState, ThreadBootstrapState, ThreadStore, ToolStepStatus};
use react_core::tools::{Tool, ToolRegistry};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
#[cfg(test)]
use crate::data_engineer::failure_classifier::ValidateFailureClass;
use crate::data_engineer::phase_actions::{
    apply_guard_block, apply_phase_transition, plan_status_reason_detail,
};

pub struct DataEngineerSuite;
const PLAN_SPEC_PLACEHOLDER_SENTINEL: &str = "__REQUIRES_PLAN_ENRICHMENT__";

pub mod controller_event;
pub mod controller_kernel;
pub mod control_flow;
pub mod dataset_truth;
pub mod dbt_error;
pub mod dbt_repair;
pub mod facts;
pub mod failure_classifier;
pub mod naming;
pub mod mutation_gateway;
pub mod patch_contract;
#[path = "patch_protocol.rs"]
pub mod files_patch_repair;
pub mod phase_actions;
pub mod phase_gate;
mod phase_author;
mod phase_plan;
mod phase_preflight;
mod phase_publish;
mod phase_review;
mod phase_validate;
pub mod plan;
pub mod ws_plans;
pub mod plan_kind;
pub mod plan_schema;
pub mod probe_target;
pub mod progress_controller;
pub mod project_files;
#[path = "project_fs/mod.rs"]
pub mod files_store;
pub mod prompt_packets;
pub mod prompts;
pub mod authoring_ir;
pub mod authoring_driver;
pub mod chunk_progress_contract;
pub mod references;
mod review_batched;
pub mod retry_budget;
pub mod schema_policy;
pub mod sql_first;
pub mod state_manager;
pub mod tool_ops;
pub mod transition_dispatcher;
pub mod tools;

fn lock_prompt_for_plan(
    kind: &str,
    plan_key: &str,
    consecutive: usize,
    total: usize,
    next_items: &[String],
    expected_paths: &[String],
) -> String {
    let track = if kind.eq_ignore_ascii_case("cleanse") {
        crate::data_engineer::controller_kernel::PlanTrack::Cleanse
    } else {
        crate::data_engineer::controller_kernel::PlanTrack::Model
    };
    crate::data_engineer::controller_kernel::build_batch_lock_prompt(
        track,
        plan_key,
        consecutive,
        total,
        next_items,
        expected_paths,
    )
}

fn batch_lock_error(reason: &str) -> String {
    let code = crate::data_engineer::controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted
        .code();
    format!("batch_locked:{}: {}", code, reason)
}

fn stable_json_digest<T: Serialize>(value: &T) -> Option<String> {
    let raw = serde_json::to_string(value).ok()?;
    Some(react_core::llm_observability::sha256_hex_str(&raw))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrackKind {
    Cleanse,
    Model,
}

impl TrackKind {
    fn from_plan_phase(phase: control_flow::Phase) -> Self {
        match phase {
            control_flow::Phase::CleansePlan => Self::Cleanse,
            control_flow::Phase::ModelPlan => Self::Model,
            _ => Self::Cleanse,
        }
    }

    fn is_cleanse(self) -> bool {
        matches!(self, Self::Cleanse)
    }

    fn author_phase(self) -> control_flow::Phase {
        match self {
            Self::Cleanse => control_flow::Phase::CleanseAuthor,
            Self::Model => control_flow::Phase::ModelAuthor,
        }
    }
}

enum PhaseExecutorOutcome {
    Continue,
    Return(Vec<FlowFrame>),
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
            "file" => 120,

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


#[derive(Clone, Debug)]
enum AllowedBatch {
    CleanseSqlDatasetIds(Vec<String>),
    CleanseSchemaDatasetIds(Vec<String>),
    ModelSqlItemNames(Vec<String>),
    ModelSchemaItemNames(Vec<String>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CatalogBootstrapOutcome {
    metadata_complete: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlanningLlmProfile {
    DiscoveryCleanse,
    DiscoveryModel,
    DesignMemo,
    DesignCritique,
    SkeletonOrCandidates,
    EnrichmentCompile,
    EnrichmentReason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum AgentMode {
    Ask,
    Model,
    Cleanse,
    Review,
    Agent,
}

impl AgentMode {
    fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "ask" => Ok(Self::Ask),
            "model" => Ok(Self::Model),
            "cleanse" => Ok(Self::Cleanse),
            "review" => Ok(Self::Review),
            "agent" => Ok(Self::Agent),
            _ => Err(format!(
                "invalid agent_type '{}' for suite 'data_engineer' (expected 'ask' | 'model' | 'cleanse' | 'review' | 'agent')",
                raw
            )),
        }
    }

}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum AgentToolCapability {
    ReadOnlyFile,
    MutableFile,
    RunSql,
    AskUser,
    AskApproval,
    SearchDbtExamples,
    StagingModel,
    GoldModel,
    DbtValidate,
    PublishDbt,
    SqlRegister,
    CatalogNote,
    Artifacts,
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
    fn agent_capability_profile(
        agent_mode: AgentMode,
        allow_user_interrupt_tools: bool,
    ) -> BTreeSet<AgentToolCapability> {
        let mut caps = BTreeSet::new();
        match agent_mode {
            AgentMode::Review => {
                caps.insert(AgentToolCapability::ReadOnlyFile);
                caps.insert(AgentToolCapability::Artifacts);
            }
            AgentMode::Ask => {
                caps.insert(AgentToolCapability::MutableFile);
                caps.insert(AgentToolCapability::RunSql);
                caps.insert(AgentToolCapability::AskApproval);
                caps.insert(AgentToolCapability::Artifacts);
            }
            AgentMode::Cleanse => {
                caps.insert(AgentToolCapability::MutableFile);
                caps.insert(AgentToolCapability::RunSql);
                caps.insert(AgentToolCapability::AskApproval);
                caps.insert(AgentToolCapability::SearchDbtExamples);
                caps.insert(AgentToolCapability::StagingModel);
                caps.insert(AgentToolCapability::DbtValidate);
                caps.insert(AgentToolCapability::PublishDbt);
                caps.insert(AgentToolCapability::SqlRegister);
                caps.insert(AgentToolCapability::CatalogNote);
                caps.insert(AgentToolCapability::Artifacts);
            }
            AgentMode::Model | AgentMode::Agent => {
                caps.insert(AgentToolCapability::MutableFile);
                caps.insert(AgentToolCapability::RunSql);
                caps.insert(AgentToolCapability::AskApproval);
                caps.insert(AgentToolCapability::SearchDbtExamples);
                caps.insert(AgentToolCapability::StagingModel);
                caps.insert(AgentToolCapability::GoldModel);
                caps.insert(AgentToolCapability::DbtValidate);
                caps.insert(AgentToolCapability::PublishDbt);
                caps.insert(AgentToolCapability::SqlRegister);
                caps.insert(AgentToolCapability::CatalogNote);
                caps.insert(AgentToolCapability::Artifacts);
            }
        }
        if allow_user_interrupt_tools && agent_mode != AgentMode::Review {
            caps.insert(AgentToolCapability::AskUser);
        }
        caps
    }

    fn headless_mode_enabled() -> bool {
        std::env::var("REACT_HEADLESS")
            .ok()
            .map(|v| {
                let t = v.trim().to_ascii_lowercase();
                !(t.is_empty() || t == "0" || t == "false" || t == "no")
            })
            .unwrap_or(false)
    }

    fn enforce_non_interactive_contract(
        agent_mode: AgentMode,
        frames: Vec<FlowFrame>,
    ) -> Result<Vec<FlowFrame>, String> {
        let await_user_prompt = frames.iter().find_map(|f| match f {
            FlowFrame::AwaitUser { prompt } => Some(prompt.clone()),
            _ => None,
        });
        if let Some(prompt) = await_user_prompt {
            if agent_mode == AgentMode::Agent {
                return Err(format!("agent_mode_await_user_forbidden: {}", prompt));
            }
            if Self::headless_mode_enabled() {
                return Err(format!("await_user_forbidden_in_headless: {}", prompt));
            }
        }
        Ok(frames)
    }

    fn build_tools_card(
        header: &str,
        tool_lines: Vec<String>,
        notes: Vec<String>,
        not_available: Option<String>,
    ) -> String {
        let mut lines: Vec<String> = Vec::new();
        lines.push(header.to_string());
        lines.extend(tool_lines);
        if !notes.is_empty() {
            lines.push(String::new());
            lines.extend(notes);
        }
        if let Some(na) = not_available {
            lines.push(String::new());
            lines.push(na);
        }
        lines.join("\n")
    }

    async fn bump_subjective_retry(
        thread_store: &ThreadStore,
        thread_id: &str,
        phase: control_flow::Phase,
        kind: crate::data_engineer::progress_controller::SubjectiveRetryKind,
    ) -> usize {
        let cap = crate::data_engineer::controller_kernel::subjective_retry_state_cap();
        let mut st = crate::data_engineer::progress_controller::ExecutionState::load(
            thread_store,
            thread_id,
        )
        .await
        .unwrap_or_else(crate::data_engineer::progress_controller::ExecutionState::new);
        let retries = st.bump_subjective_retry(phase, kind, cap);
        if let Err(e) = st.save(thread_store, thread_id).await {
            tracing::warn!("failed to persist execution_state subjective retry: {}", e);
        }
        retries
    }

    async fn reset_subjective_retry(thread_store: &ThreadStore, thread_id: &str) {
        let mut st = crate::data_engineer::progress_controller::ExecutionState::load(
            thread_store,
            thread_id,
        )
        .await
        .unwrap_or_else(crate::data_engineer::progress_controller::ExecutionState::new);
        st.reset_subjective_retry();
        if let Err(e) = st.save(thread_store, thread_id).await {
            tracing::warn!("failed to reset execution_state subjective retry: {}", e);
        }
    }

    fn should_reset_subjective_retry_after_plan_save(entered_from_actionable_review: bool) -> bool {
        // Preserve retry lifecycle when review requested plan edits; this keeps loopback guards
        // monotonic across cleanse/model/publish review feedback cycles.
        !entered_from_actionable_review
    }

    fn planning_llm_options(
        profile: PlanningLlmProfile,
        prompt_id: &'static str,
        thread_id: Option<String>,
    ) -> LlmCallOptions {
        match profile {
            PlanningLlmProfile::DiscoveryCleanse => {
                let max_tokens = std::env::var("LLM_PLAN_MAX_TOKENS_CLEANSE")
                    .ok()
                    .and_then(|s| s.parse::<u32>().ok())
                    .unwrap_or(96_000)
                    .max(4_000);
                let reasoning_effort =
                    Self::parse_reasoning_effort_env("LLM_PLAN_REASONING_EFFORT_CLEANSE")
                        .or_else(|| Self::parse_reasoning_effort_env("LLM_PLAN_REASONING_EFFORT"))
                        .unwrap_or(react_core::llm::ReasoningEffort::Medium);
                LlmCallOptions {
                    prompt_id,
                    thread_id,
                    expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
                    temperature: Some(0.20),
                    top_p: Some(1.0),
                    max_output_tokens: Some(max_tokens),
                    reasoning_effort: Some(reasoning_effort),
                }
            }
            PlanningLlmProfile::DiscoveryModel => {
                let max_tokens = std::env::var("LLM_PLAN_MAX_TOKENS_MODEL")
                    .ok()
                    .and_then(|s| s.parse::<u32>().ok())
                    .unwrap_or(128_000)
                    .max(8_000);
                let reasoning_effort =
                    Self::parse_reasoning_effort_env("LLM_PLAN_REASONING_EFFORT_MODEL")
                        .or_else(|| Self::parse_reasoning_effort_env("LLM_PLAN_REASONING_EFFORT"))
                        .unwrap_or(react_core::llm::ReasoningEffort::Medium);
                LlmCallOptions {
                    prompt_id,
                    thread_id,
                    expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
                    temperature: Some(0.55),
                    top_p: Some(0.95),
                    max_output_tokens: Some(max_tokens),
                    reasoning_effort: Some(reasoning_effort),
                }
            }
            PlanningLlmProfile::DesignMemo => {
                let max_tokens = std::env::var("LLM_PLAN_MEMO_MAX_TOKENS")
                    .ok()
                    .and_then(|s| s.parse::<u32>().ok())
                    .unwrap_or(64_000)
                    .max(8_000);
                let reasoning_effort =
                    Self::parse_reasoning_effort_env("LLM_PLAN_REASONING_EFFORT")
                        .unwrap_or(react_core::llm::ReasoningEffort::Medium);
                LlmCallOptions {
                    prompt_id,
                    thread_id,
                    expected_format: react_core::llm::LlmExpectedFormat::Text,
                    temperature: Some(0.2),
                    top_p: Some(1.0),
                    max_output_tokens: Some(max_tokens),
                    reasoning_effort: Some(reasoning_effort),
                }
            }
            PlanningLlmProfile::DesignCritique => {
                let max_tokens = std::env::var("LLM_PLAN_CRITIQUE_MAX_TOKENS")
                    .ok()
                    .and_then(|s| s.parse::<u32>().ok())
                    .unwrap_or(16_000)
                    .max(4_000);
                LlmCallOptions {
                    prompt_id,
                    thread_id,
                    expected_format: react_core::llm::LlmExpectedFormat::JsonSchemaSpec {
                        name: "suite.plan_design_critique.v1".to_string(),
                        schema: crate::data_engineer::plan_schema::strict_schema_for::<
                            crate::data_engineer::plan_schema::PlanDesignCritiqueV1,
                        >(),
                    },
                    temperature: Some(0.10),
                    top_p: Some(1.0),
                    max_output_tokens: Some(max_tokens),
                    reasoning_effort: Some(react_core::llm::ReasoningEffort::Low),
                }
            }
            PlanningLlmProfile::SkeletonOrCandidates => {
                let max_tokens = std::env::var("LLM_PLAN_SKELETON_MAX_TOKENS")
                    .ok()
                    .and_then(|s| s.parse::<u32>().ok())
                    .unwrap_or(24_000)
                    .max(6_000);
                LlmCallOptions {
                    prompt_id,
                    thread_id,
                    expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
                    temperature: Some(0.1),
                    top_p: Some(1.0),
                    max_output_tokens: Some(max_tokens),
                    reasoning_effort: Some(react_core::llm::ReasoningEffort::Low),
                }
            }
            PlanningLlmProfile::EnrichmentCompile => {
                let max_tokens = std::env::var("LLM_PLAN_ENRICH_MAX_TOKENS")
                    .ok()
                    .and_then(|s| s.parse::<u32>().ok())
                    .unwrap_or(32_000)
                    .max(6_000);
                LlmCallOptions {
                    prompt_id,
                    thread_id,
                    expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
                    temperature: Some(0.10),
                    top_p: Some(1.0),
                    max_output_tokens: Some(max_tokens),
                    reasoning_effort: Some(react_core::llm::ReasoningEffort::Low),
                }
            }
            PlanningLlmProfile::EnrichmentReason => {
                let max_tokens = std::env::var("LLM_PLAN_ENRICH_REASON_MAX_TOKENS")
                    .ok()
                    .and_then(|s| s.parse::<u32>().ok())
                    .unwrap_or(8_000)
                    .max(2_000);
                LlmCallOptions {
                    prompt_id,
                    thread_id,
                    expected_format: react_core::llm::LlmExpectedFormat::Text,
                    temperature: Some(0.20),
                    top_p: Some(1.0),
                    max_output_tokens: Some(max_tokens),
                    reasoning_effort: Some(react_core::llm::ReasoningEffort::Low),
                }
            }
        }
    }

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

    fn enforce_cleanse_plan_raw_only(plan: &mut crate::data_engineer::plan::CleansePlan) -> usize {
        let before = plan.tasks.len();
        plan.tasks
            .retain(|t| Self::is_raw_dataset_id(t.dataset_id.trim()));
        let keep: HashSet<String> = plan
            .tasks
            .iter()
            .map(|t| t.dataset_id.trim().to_string())
            .collect();
        for b in plan.batches.iter_mut() {
            b.retain(|ds| keep.contains(ds.trim()));
        }
        plan.batches.retain(|b| !b.is_empty());
        before.saturating_sub(plan.tasks.len())
    }

    fn synthesize_cleanse_plan_from_grounded_raw(
        plan: &mut crate::data_engineer::plan::CleansePlan,
        allowed_raw: &BTreeSet<String>,
    ) -> bool {
        if allowed_raw.is_empty() {
            return false;
        }
        let ids: Vec<String> = allowed_raw.iter().cloned().collect();
        let tasks = ids
            .iter()
            .map(|dataset_id| crate::data_engineer::plan::CleanseTask {
                dataset_id: dataset_id.clone(),
                expected_model_path: None,
                invariants: vec![],
                implementation_spec: crate::data_engineer::plan::CleanseImplementationSpec {
                    spec_version: 1,
                    row_preserving: true,
                    output_fields: vec![],
                    prohibited_ops: vec![],
                },
                status: crate::data_engineer::plan::TaskStatus::Pending,
                checklist: crate::data_engineer::plan::canonical_task_checklist(true),
            })
            .collect::<Vec<_>>();
        let batches = ids.chunks(5).map(|c| c.to_vec()).collect::<Vec<_>>();
        plan.tasks = tasks;
        plan.batches = batches;
        plan.work_groups = crate::data_engineer::plan::canonical_work_groups_from_batches(
            &plan.batches,
            "cleanse",
        );
        true
    }

    fn collect_cleanse_grounding_candidates(
        plan: &crate::data_engineer::plan::CleansePlan,
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
    ) -> Result<crate::data_engineer::plan_schema::CleansePlanSkeletonV1, String> {
        let ids = NonEmptyCleanseDatasetIds::from_discovered_raw(discovered_raw)?;
        let tasks = ids
            .as_slice()
            .iter()
            .map(|dataset_id| crate::data_engineer::plan_schema::CleansePlanSkeletonTaskV1 {
                dataset_id: dataset_id.clone(),
            })
            .collect::<Vec<_>>();
        let batches = ids
            .as_slice()
            .chunks(5)
            .map(|chunk| chunk.to_vec())
            .collect::<Vec<_>>();
        Ok(crate::data_engineer::plan_schema::CleansePlanSkeletonV1 { tasks, batches })
    }

    fn is_raw_dataset_id(dataset_id: &str) -> bool {
        let Some(ds) = crate::data_engineer::references::DatasetRef::parse(dataset_id) else {
            return false;
        };
        let schema = ds.schema.to_ascii_lowercase();
        let table = ds.table.to_ascii_lowercase();
        schema.contains("raw") || table.starts_with("raw_")
    }


    async fn discovered_raw_relations_from_catalog(
        datasets: Option<&Arc<dyn react_core::providers::DatasetCatalogProvider>>,
    ) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let Some(ds) = datasets else {
            return out;
        };
        let Ok(items) = ds.list_datasets().await else {
            return out;
        };
        for item in items {
            let fqn = item.fqn();
            if Self::is_raw_dataset_id(&fqn) {
                out.insert(fqn);
            }
        }
        out
    }

    async fn run_deterministic_probe_for_table(
        thread_store: &ThreadStore,
        thread_id: &str,
        actx: &AgentCtx,
        sql_schema_tool: &tools::sql_schema::SqlSchemaTool,
        sql_stats_tool: &tools::sql_stats::SqlStatsTool,
        sql_sample_tool: &tools::sql_sample::SqlSampleTool,
        run_sql_tool: &tools::sql_run::SqlRunTool,
        table: &str,
        sql_schema_timeout: u64,
        sql_stats_timeout: u64,
        sql_sample_timeout: u64,
        run_sql_timeout: u64,
    ) -> (bool, Option<String>) {
        let schema_obs = control_flow::call_and_record_tool(
            thread_store,
            thread_id,
            Some("agent".to_string()),
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
                Some("agent".to_string()),
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
            let sample_obs = control_flow::call_and_record_tool(
                thread_store,
                thread_id,
                Some("agent".to_string()),
                sql_sample_tool,
                serde_json::json!({"table": table, "field": field, "k": 10}),
                actx,
                sql_sample_timeout,
            )
            .await;
            if sample_obs
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
            Some("agent".to_string()),
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

    fn review_retry_kind(
        decision: ReviewDecision,
    ) -> Option<crate::data_engineer::progress_controller::SubjectiveRetryKind> {
        match decision {
            ReviewDecision::PatchPlan => Some(
                crate::data_engineer::progress_controller::SubjectiveRetryKind::ReviewPatchPlan,
            ),
            ReviewDecision::PatchImpl => Some(
                crate::data_engineer::progress_controller::SubjectiveRetryKind::ReviewPatchImpl,
            ),
            ReviewDecision::Proceed => None,
        }
    }

    fn churn_audit_acceptance_criteria() -> serde_json::Value {
        serde_json::json!({
            "no_false_completion": "plans are never marked completed while checklist items remain pending",
            "no_non_executable_authoring": "authoring exits to planning when executable work-group structure is invalid",
            "no_review_churn_from_missing_staging": "model plan grounding loops must not be caused by skipped upstream authoring",
        })
    }

    async fn ensure_catalog_bootstrap_semaphored(
        thread_id: &str,
        sctx: &SuiteCtx,
    ) -> Result<(), String> {
        let thread_store = ThreadStore::new(
            sctx.storage.clone(),
            sctx.scope.clone(),
            sctx.keyspace.clone(),
        );
        if let Ok(st) = thread_store.get_thread_state(thread_id).await {
            if let Some(catalog) = st.bootstrap.catalog.as_ref() {
                if catalog.status == "ready" || catalog.status == "best_effort" {
                    tracing::info!(
                        "data_engineer: catalog bootstrap semaphore hit status={} thread_id={}",
                        catalog.status,
                        thread_id
                    );
                    return Ok(());
                }
            }
        }
        let out = Self::ensure_catalog_bootstrap(sctx).await?;
        let status = if out.metadata_complete {
            "ready".to_string()
        } else {
            "best_effort".to_string()
        };
        let ts = chrono::Utc::now().to_rfc3339();
        let mut st = thread_store
            .get_thread_state(thread_id)
            .await
            .unwrap_or_else(|_| react_core::session::ThreadState {
                thread_state_schema_version: react_core::session::THREAD_STATE_SCHEMA_VERSION,
                thread_id: thread_id.to_string(),
                ..react_core::session::ThreadState::default()
            });
        st.bootstrap = ThreadBootstrapState {
            catalog: Some(CatalogBootstrapState {
                status,
                metadata_complete: out.metadata_complete,
                ts,
            }),
        };
        if let Err(e) = thread_store.put_thread_state(thread_id, &st).await {
            tracing::warn!(
                "data_engineer: failed to persist catalog bootstrap state thread_id={} err={}",
                thread_id,
                e
            );
        }
        Ok(())
    }

    fn parse_reasoning_effort_env(var: &str) -> Option<react_core::llm::ReasoningEffort> {
        match std::env::var(var)
            .ok()
            .map(|s| s.trim().to_lowercase())
            .as_deref()
        {
            Some("none") => Some(react_core::llm::ReasoningEffort::None),
            Some("low") => Some(react_core::llm::ReasoningEffort::Low),
            Some("medium") => Some(react_core::llm::ReasoningEffort::Medium),
            Some("high") => Some(react_core::llm::ReasoningEffort::High),
            _ => None,
        }
    }

    fn excerpt(s: &str, max_chars: usize) -> String {
        if s.chars().count() <= max_chars {
            return s.to_string();
        }
        let mut out = s.chars().take(max_chars).collect::<String>();
        out.push_str("\n... (truncated)");
        out
    }

    fn parse_json_object_lenient(raw: &str) -> Result<serde_json::Value, String> {
        // Hard cutover: strict JSON object parsing only.
        serde_json::from_str::<serde_json::Value>(raw).map_err(|e| e.to_string())
    }

    fn parse_json_typed_lenient<T: serde::de::DeserializeOwned>(raw: &str) -> Result<T, String> {
        let v = Self::parse_json_object_lenient(raw)?;
        serde_json::from_value::<T>(v).map_err(|e| e.to_string())
    }

    fn sanitize_impl_spec_value(
        mut v: serde_json::Value,
        is_cleanse: bool,
    ) -> (serde_json::Value, Vec<String>) {
        let mut stripped: Vec<String> = Vec::new();
        let Some(obj) = v.as_object_mut() else {
            return (v, stripped);
        };
        let allowed: HashSet<&'static str> = if is_cleanse {
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
        is_cleanse: bool,
    ) -> Result<(T, Vec<String>), String> {
        let (sv, stripped) = Self::sanitize_impl_spec_value(v, is_cleanse);
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
        snapshot: &mut serde_json::Value,
        key: &str,
        event: serde_json::Value,
        max_len: usize,
    ) {
        if snapshot.is_null() {
            *snapshot = serde_json::json!({});
        }
        if let Some(obj) = snapshot.as_object_mut() {
            let arr = obj
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

    fn plan_enrich_chunk_size() -> usize {
        std::env::var("LLM_PLAN_ENRICH_CHUNK_SIZE")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(3)
            .clamp(1, 3)
    }

    async fn generate_design_memo(
        ctx: &AgentCtx,
        is_cleanse: bool,
        planning_context: &str,
    ) -> Result<String, String> {
        use react_core::llm::ChatMessage;
        let kind = if is_cleanse {
            "cleanse_plan"
        } else {
            "model_plan"
        };
        let sys = crate::prompts::plan::plan_design_memo_system_prompt(kind);
        let user = format!(
            "Planning kind: {kind}\n\nContext:\n{}\n\nWrite the design memo.",
            Self::excerpt(planning_context, 120_000)
        );
        let opts = Self::planning_llm_options(
            PlanningLlmProfile::DesignMemo,
            "data_engineer.plan_design_memo",
            ctx.thread_id.clone(),
        );
        ctx.llm
            .chat(
                &[
                    ChatMessage {
                        role: "system".to_string(),
                        content: sys,
                    },
                    ChatMessage {
                        role: "user".to_string(),
                        content: user,
                    },
                ],
                &opts,
            )
            .map_err(|e| e.to_string())
    }

    async fn critique_design_memo(
        ctx: &AgentCtx,
        is_cleanse: bool,
        planning_context: &str,
        memo: &str,
    ) -> Result<crate::data_engineer::plan_schema::PlanDesignCritiqueV1, String> {
        use react_core::llm::ChatMessage;
        let kind = if is_cleanse {
            "cleanse_plan"
        } else {
            "model_plan"
        };
        let opts = Self::planning_llm_options(
            PlanningLlmProfile::DesignCritique,
            "data_engineer.plan_design_critique",
            ctx.thread_id.clone(),
        );
        let sys = crate::prompts::plan::plan_design_critique_system_prompt(kind);
        let user = format!(
            "Planning kind: {kind}\n\nContext:\n{}\n\nDesign memo:\n{}\n\nReturn critique JSON.",
            Self::excerpt(planning_context, 80_000),
            Self::excerpt(memo, 40_000)
        );
        let raw = ctx.llm.chat(
            &[
                ChatMessage {
                    role: "system".to_string(),
                    content: sys,
                },
                ChatMessage {
                    role: "user".to_string(),
                    content: user,
                },
            ],
            &opts,
        )?;
        Self::parse_json_typed_lenient::<crate::data_engineer::plan_schema::PlanDesignCritiqueV1>(&raw)
    }

    async fn revise_design_memo(
        ctx: &AgentCtx,
        is_cleanse: bool,
        planning_context: &str,
        memo: &str,
        critique: &crate::data_engineer::plan_schema::PlanDesignCritiqueV1,
    ) -> Result<String, String> {
        use react_core::llm::ChatMessage;
        let kind = if is_cleanse {
            "cleanse_plan"
        } else {
            "model_plan"
        };
        let sys = crate::prompts::plan::plan_design_memo_system_prompt(kind);
        let user = format!(
            "Planning kind: {kind}\n\nContext:\n{}\n\nCurrent design memo:\n{}\n\nCritique JSON:\n{}\n\nRewrite the design memo in free text so the critique blockers/fixes are addressed.\nDo not return JSON.",
            Self::excerpt(planning_context, 90_000),
            Self::excerpt(memo, 40_000),
            serde_json::to_string_pretty(critique).unwrap_or_else(|_| "{}".to_string()),
        );
        let opts = Self::planning_llm_options(
            PlanningLlmProfile::DesignMemo,
            "data_engineer.plan_design_memo_revise",
            ctx.thread_id.clone(),
        );
        ctx.llm
            .chat(
                &[
                    ChatMessage {
                        role: "system".to_string(),
                        content: sys,
                    },
                    ChatMessage {
                        role: "user".to_string(),
                        content: user,
                    },
                ],
                &opts,
            )
            .map_err(|e| e.to_string())
    }

    async fn produce_critiqued_design_memo(
        ctx: &AgentCtx,
        is_cleanse: bool,
        planning_context: &str,
    ) -> Result<
        (
            String,
            crate::data_engineer::plan_schema::PlanDesignCritiqueV1,
        ),
        String,
    > {
        let mut memo = Self::generate_design_memo(ctx, is_cleanse, planning_context).await?;
        let mut critique = Self::critique_design_memo(ctx, is_cleanse, planning_context, &memo)
            .await?;
        // Bounded revision loop: critique feedback must update memo reasoning before extraction.
        for _ in 0..1 {
            if critique.ok {
                break;
            }
            memo =
                Self::revise_design_memo(ctx, is_cleanse, planning_context, &memo, &critique)
                    .await?;
            critique = Self::critique_design_memo(ctx, is_cleanse, planning_context, &memo)
                .await?;
        }
        Ok((memo, critique))
    }

    fn critique_guidance(critique: &crate::data_engineer::plan_schema::PlanDesignCritiqueV1) -> String {
        if critique.blockers.is_empty() && critique.fixes.is_empty() {
            return "Design critique: no blockers identified.".to_string();
        }
        let blockers = if critique.blockers.is_empty() {
            "- (none)".to_string()
        } else {
            critique
                .blockers
                .iter()
                .take(6)
                .map(|b| {
                    let target = b
                        .target_id
                        .as_deref()
                        .map(|s| format!(" target={}", s))
                        .unwrap_or_default();
                    let detail = b
                        .detail
                        .as_deref()
                        .map(|s| format!(" detail={}", s.trim()))
                        .unwrap_or_default();
                    format!("- {:?}{}{}", b.code, target, detail)
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let fixes = if critique.fixes.is_empty() {
            "- (none)".to_string()
        } else {
            critique
                .fixes
                .iter()
                .take(6)
                .map(|f| {
                    let blocker = f
                        .blocker_code
                        .map(|c| format!(" blocker={:?}", c))
                        .unwrap_or_default();
                    let target = f
                        .target_id
                        .as_deref()
                        .map(|s| format!(" target={}", s))
                        .unwrap_or_default();
                    let detail = f
                        .detail
                        .as_deref()
                        .map(|s| format!(" detail={}", s.trim()))
                        .unwrap_or_default();
                    format!("- {:?}{}{}{}", f.action, blocker, target, detail)
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        format!(
            "Design critique (bounded one-pass):\n\
ok={}\n\
blockers:\n{}\n\
fixes:\n{}\n\
Apply these fixes in the output.",
            critique.ok, blockers, fixes
        )
    }

    async fn generate_model_candidates(
        ctx: &AgentCtx,
        planning_context: &str,
        memo: &str,
        critique: &crate::data_engineer::plan_schema::PlanDesignCritiqueV1,
    ) -> Result<crate::data_engineer::plan_schema::ModelPlanCandidatesV1, String> {
        use react_core::llm::ChatMessage;
        let mut opts = Self::planning_llm_options(
            PlanningLlmProfile::SkeletonOrCandidates,
            "data_engineer.model_plan_candidates",
            ctx.thread_id.clone(),
        );
        opts.expected_format = react_core::llm::LlmExpectedFormat::JsonSchemaSpec {
            name: "suite.model_plan_candidates.v1".to_string(),
            schema: crate::data_engineer::plan_schema::strict_schema_for::<
                crate::data_engineer::plan_schema::ModelPlanCandidatesV1,
            >(),
        };
        let sys = crate::prompts::plan::model_plan_candidates_system_prompt();
        let user = format!(
            "Context:\n{}\n\nDesign memo:\n{}\n\n{}\n\nReturn candidate-selection JSON.",
            Self::excerpt(planning_context, 60_000),
            Self::excerpt(memo, 30_000),
            Self::critique_guidance(critique)
        );
        let raw = ctx.llm.chat(
            &[
                ChatMessage {
                    role: "system".to_string(),
                    content: sys.to_string(),
                },
                ChatMessage {
                    role: "user".to_string(),
                    content: user,
                },
            ],
            &opts,
        )?;
        Self::parse_json_typed_lenient::<crate::data_engineer::plan_schema::ModelPlanCandidatesV1>(&raw)
    }

    fn placeholder_output_field(name: &str) -> crate::data_engineer::plan::OutputFieldSpec {
        crate::data_engineer::plan::OutputFieldSpec {
            name: name.to_string(),
            kind: crate::data_engineer::plan::FieldKind::Raw,
            source_columns: vec![],
            expression: PLAN_SPEC_PLACEHOLDER_SENTINEL.to_string(),
            data_type: None,
            nullable: false,
            description: Some("Deterministic placeholder; must be replaced by plan enrichment.".to_string()),
        }
    }

    fn placeholder_cleanse_impl_spec() -> crate::data_engineer::plan::CleanseImplementationSpec {
        crate::data_engineer::plan::CleanseImplementationSpec {
            spec_version: 1,
            row_preserving: true,
            output_fields: vec![Self::placeholder_output_field("__replace_me__")],
            prohibited_ops: vec![PLAN_SPEC_PLACEHOLDER_SENTINEL.to_string()],
        }
    }

    fn placeholder_model_impl_spec() -> crate::data_engineer::plan::ModelImplementationSpec {
        crate::data_engineer::plan::ModelImplementationSpec {
            spec_version: 1,
            grain: PLAN_SPEC_PLACEHOLDER_SENTINEL.to_string(),
            inputs: vec![],
            joins: vec![],
            metrics: vec![],
            output_fields: vec![Self::placeholder_output_field("__replace_me__")],
            assumptions: vec![PLAN_SPEC_PLACEHOLDER_SENTINEL.to_string()],
        }
    }

    fn is_placeholder_cleanse_spec(spec: &crate::data_engineer::plan::CleanseImplementationSpec) -> bool {
        spec.prohibited_ops
            .iter()
            .any(|v| v == PLAN_SPEC_PLACEHOLDER_SENTINEL)
            || spec
                .output_fields
                .iter()
                .any(|f| f.expression == PLAN_SPEC_PLACEHOLDER_SENTINEL)
    }

    fn is_placeholder_model_spec(spec: &crate::data_engineer::plan::ModelImplementationSpec) -> bool {
        spec.grain == PLAN_SPEC_PLACEHOLDER_SENTINEL
            || spec
                .assumptions
                .iter()
                .any(|v| v == PLAN_SPEC_PLACEHOLDER_SENTINEL)
            || spec
                .output_fields
                .iter()
                .any(|f| f.expression == PLAN_SPEC_PLACEHOLDER_SENTINEL)
    }

    fn compile_cleanse_skeleton_plan(
        skeleton: &crate::data_engineer::plan_schema::CleansePlanSkeletonV1,
    ) -> crate::data_engineer::plan::CleansePlan {
        let task_ids: Vec<String> = skeleton
            .tasks
            .iter()
            .map(|t| t.dataset_id.trim().to_string())
            .filter(|id| !id.is_empty())
            .collect();
        let tasks: Vec<crate::data_engineer::plan::CleanseTask> = task_ids
            .iter()
            .map(|dataset_id| {
                crate::data_engineer::plan::CleanseTask {
                    dataset_id: dataset_id.to_string(),
                    expected_model_path: None,
                    invariants: vec![],
                    implementation_spec: Self::placeholder_cleanse_impl_spec(),
                    status: Default::default(),
                    checklist: crate::data_engineer::plan::canonical_task_checklist(true),
                }
            })
            .collect();
        let batches: Vec<Vec<String>> = if skeleton.batches.is_empty() {
            task_ids.chunks(5).map(|c| c.to_vec()).collect()
        } else {
            skeleton.batches.clone()
        };
        let work_groups =
            crate::data_engineer::plan::canonical_work_groups_from_batches(&batches, "cleanse");
        crate::data_engineer::plan::CleansePlan {
            plan_key: String::new(),
            status: crate::data_engineer::plan::PlanStatus::Draft,
            project_snapshot: serde_json::json!({}),
            tasks,
            batches,
            work_groups,
            mutations: vec![],
            progress: Default::default(),
        }
    }

    fn model_plan_min_score() -> i32 {
        std::env::var("LLM_MODEL_PLAN_MIN_SCORE")
            .ok()
            .and_then(|s| s.parse::<i32>().ok())
            .map(|p| p.clamp(0, 100))
            .unwrap_or(70)
    }

    fn select_high_value_model_candidates(
        candidates: &[crate::data_engineer::plan_schema::ModelPlanCandidateV1],
    ) -> Vec<crate::data_engineer::plan_schema::ModelPlanCandidateV1> {
        if candidates.is_empty() {
            return vec![];
        }
        let min_score = Self::model_plan_min_score();
        let mut ranked = candidates.to_vec();
        ranked.sort_by(|a, b| {
            b.value_score
                .cmp(&a.value_score)
                .then_with(|| a.name.cmp(&b.name))
        });
        ranked
            .into_iter()
            .filter(|c| c.value_score >= min_score)
            .collect()
    }

    fn compile_model_candidates_plan(
        candidates: &crate::data_engineer::plan_schema::ModelPlanCandidatesV1,
    ) -> crate::data_engineer::plan::ModelPlan {
        let selected = Self::select_high_value_model_candidates(&candidates.candidates);
        let task_names: Vec<String> = selected.into_iter().map(|c| c.name).collect();
        let batches: Vec<Vec<String>> = task_names.chunks(5).map(|c| c.to_vec()).collect();
        let tasks: Vec<crate::data_engineer::plan::ModelTask> = task_names
            .into_iter()
            .map(|name| {
                crate::data_engineer::plan::ModelTask {
                    name,
                    folder: String::new(),
                    goal: String::new(),
                    inputs: vec![],
                    expected_model_path: None,
                    invariants: vec![],
                    implementation_spec: Self::placeholder_model_impl_spec(),
                    status: Default::default(),
                    checklist: crate::data_engineer::plan::canonical_task_checklist(false),
                }
            })
            .collect();
        let work_groups =
            crate::data_engineer::plan::canonical_work_groups_from_batches(&batches, "model");
        crate::data_engineer::plan::ModelPlan {
            plan_key: String::new(),
            status: crate::data_engineer::plan::PlanStatus::Draft,
            project_snapshot: serde_json::json!({}),
            tasks,
            batches,
            work_groups,
            mutations: vec![],
            progress: Default::default(),
        }
    }

    fn apply_cleanse_enrichment_items(
        plan: &mut crate::data_engineer::plan::CleansePlan,
        allowed_task_ids: &[String],
        items: Vec<crate::data_engineer::plan_schema::CleansePlanEnrichmentItemV1>,
    ) -> (Vec<String>, Vec<String>) {
        let mut failed: Vec<String> = Vec::new();
        let mut failure_errors: Vec<String> = Vec::new();
        for it in items {
            if !allowed_task_ids.iter().any(|t| t == &it.task_id) {
                continue;
            }
            let spec_value = match serde_json::to_value(&it.implementation_spec) {
                Ok(v) => v,
                Err(e) => {
                    failed.push(it.task_id);
                    failure_errors.push(e.to_string());
                    continue;
                }
            };
            match Self::parse_impl_spec_value_with_sanitize::<
                crate::data_engineer::plan::CleanseImplementationSpec,
            >(spec_value, true)
            {
                Ok((spec, stripped)) => {
                    if !stripped.is_empty() {
                        Self::push_snapshot_array_event(
                            &mut plan.project_snapshot,
                            "spec_sanitizer_events",
                            serde_json::json!({
                                "phase": "cleanse_enrich",
                                "task_id": it.task_id,
                                "stripped_keys": stripped
                            }),
                            200,
                        );
                    }
                    if let Some(t) = plan.tasks.iter_mut().find(|t| t.dataset_id == it.task_id) {
                        t.implementation_spec = spec;
                    }
                }
                Err(e) => {
                    failed.push(it.task_id);
                    failure_errors.push(e);
                }
            }
        }
        (failed, failure_errors)
    }

    fn apply_model_enrichment_items(
        plan: &mut crate::data_engineer::plan::ModelPlan,
        allowed_task_ids: &[String],
        items: Vec<crate::data_engineer::plan_schema::ModelPlanEnrichmentItemV1>,
    ) -> (Vec<String>, Vec<String>) {
        let mut failed: Vec<String> = Vec::new();
        let mut failure_errors: Vec<String> = Vec::new();
        for it in items {
            if !allowed_task_ids.iter().any(|t| t == &it.task_id) {
                continue;
            }
            let spec_value = match serde_json::to_value(&it.implementation_spec) {
                Ok(v) => v,
                Err(e) => {
                    failed.push(it.task_id);
                    failure_errors.push(e.to_string());
                    continue;
                }
            };
            match Self::parse_impl_spec_value_with_sanitize::<
                crate::data_engineer::plan::ModelImplementationSpec,
            >(spec_value, false)
            {
                Ok((spec, stripped)) => {
                    if !stripped.is_empty() {
                        Self::push_snapshot_array_event(
                            &mut plan.project_snapshot,
                            "spec_sanitizer_events",
                            serde_json::json!({
                                "phase": "model_enrich",
                                "task_id": it.task_id,
                                "stripped_keys": stripped
                            }),
                            200,
                        );
                    }
                    if let Some(t) = plan.tasks.iter_mut().find(|t| t.name == it.task_id) {
                        let spec_inputs = spec.inputs.clone();
                        t.implementation_spec = spec;
                        if !spec_inputs.is_empty() {
                            t.inputs = spec_inputs;
                        }
                        if t.goal.trim().is_empty() {
                            t.goal =
                                format!("Build {} from grounded staging inputs.", t.name.trim());
                        }
                    }
                }
                Err(e) => {
                    failed.push(it.task_id);
                    failure_errors.push(e);
                }
            }
        }
        (failed, failure_errors)
    }

    fn build_enrichment_prompt_envelope(
        phase: control_flow::Phase,
        directive: crate::data_engineer::prompt_packets::TurnDirective,
        plan_kind: crate::data_engineer::plan_kind::PlanKind,
        plan_key: &str,
        planning_context: &str,
        memo: &str,
        critique: &crate::data_engineer::plan_schema::PlanDesignCritiqueV1,
        plan_summary: &str,
        task_ids: &[String],
        new_evidence_refs: &[String],
    ) -> String {
        let context_text = format!(
            "Context:\n{}\n\nDesign memo:\n{}\n\n{}\n\nCurrent plan summary:\n{}",
            Self::excerpt(planning_context, 20_000),
            Self::excerpt(memo, 12_000),
            Self::critique_guidance(critique),
            plan_summary
        );
        let envelope = crate::data_engineer::prompt_packets::PromptEnvelope {
            phase: phase.as_str().to_string(),
            goal: "Produce implementation_spec content for target tasks".to_string(),
            directive,
            plan: Some(crate::data_engineer::prompt_packets::PlanContextPacket {
                plan_kind: Some(plan_kind),
                plan_key: Some(plan_key.to_string()),
                context_text: Some(context_text),
                unresolved_ids: task_ids.to_vec(),
                new_evidence_refs: new_evidence_refs.to_vec(),
            }),
            ..crate::data_engineer::prompt_packets::PromptEnvelope::default()
        };
        crate::data_engineer::prompt_packets::render_envelope(&envelope)
            .unwrap_or_else(|_| "{}".to_string())
    }

    fn compile_prompt_from_reason(reason_memo: &str, base_user: &str) -> String {
        format!(
            "Reason memo:\n{}\n\n{}",
            Self::excerpt(reason_memo, 8_000),
            base_user
        )
    }

    async fn enrich_cleanse_tasks(
        ctx: &AgentCtx,
        planning_context: &str,
        memo: &str,
        critique: &crate::data_engineer::plan_schema::PlanDesignCritiqueV1,
        plan: &mut crate::data_engineer::plan::CleansePlan,
        task_ids: &[String],
    ) -> Result<(), String> {
        use react_core::llm::ChatMessage;
        for chunk in task_ids.chunks(Self::plan_enrich_chunk_size()) {
            let chunk_vec = chunk.to_vec();
            let summary = crate::data_engineer::plan::summarize_cleanse_plan(plan, 50);
            let base_user = format!(
                "{}\n\nTarget task_ids:\n{}\n\nReturn schema-valid enrichment JSON.",
                Self::build_enrichment_prompt_envelope(
                    control_flow::Phase::CleansePlan,
                    crate::data_engineer::prompt_packets::TurnDirective::Compile,
                    crate::data_engineer::plan_kind::PlanKind::Cleanse,
                    &plan.plan_key,
                    planning_context,
                    memo,
                    critique,
                    &summary,
                    &chunk_vec,
                    &[],
                ),
                serde_json::to_string_pretty(&chunk_vec).unwrap_or_else(|_| "[]".to_string())
            );
            let reason_user = format!(
                "Think through the enrichment strategy for these task_ids. Return plain text only, no JSON.\n\n{}",
                Self::build_enrichment_prompt_envelope(
                    control_flow::Phase::CleansePlan,
                    crate::data_engineer::prompt_packets::TurnDirective::Reason,
                    crate::data_engineer::plan_kind::PlanKind::Cleanse,
                    &plan.plan_key,
                    planning_context,
                    memo,
                    critique,
                    &summary,
                    &chunk_vec,
                    &[],
                )
            );
            let reason_memo = ctx.llm.chat(
                &[
                    ChatMessage {
                        role: "system".to_string(),
                        content: crate::prompts::plan::plan_enrichment_reason_system_prompt(),
                    },
                    ChatMessage {
                        role: "user".to_string(),
                        content: reason_user,
                    },
                ],
                &Self::planning_llm_options(
                    PlanningLlmProfile::EnrichmentReason,
                    "data_engineer.cleanse_plan_enrich_reason",
                    ctx.thread_id.clone(),
                ),
            )?;
            let compile_user = Self::compile_prompt_from_reason(&reason_memo, &base_user);
            let mut opts = Self::planning_llm_options(
                PlanningLlmProfile::EnrichmentCompile,
                "data_engineer.cleanse_plan_enrich",
                ctx.thread_id.clone(),
            );
            opts.expected_format = react_core::llm::LlmExpectedFormat::JsonSchemaSpec {
                name: "suite.cleanse_plan_enrichment.v1".to_string(),
                schema: crate::data_engineer::plan_schema::strict_schema_for::<
                    crate::data_engineer::plan_schema::CleansePlanEnrichmentV1,
                >(),
            };
            let raw = ctx.llm.chat(
                &[
                    ChatMessage {
                        role: "system".to_string(),
                        content: crate::prompts::plan::cleanse_plan_enrichment_system_prompt(),
                    },
                    ChatMessage {
                        role: "user".to_string(),
                        content: compile_user,
                    },
                ],
                &opts,
            )?;
            let enrich = Self::parse_json_typed_lenient::<
                crate::data_engineer::plan_schema::CleansePlanEnrichmentV1,
            >(&raw)?;
            let (failed, failure_errors) =
                Self::apply_cleanse_enrichment_items(plan, &chunk_vec, enrich.items);
            if !failed.is_empty() {
                let retry_hint = format!(
                    "You previously returned invalid implementation_spec.\nErrors:\n{}\nOnly emit implementation_spec object with keys: spec_version,row_preserving,output_fields,prohibited_ops.\noutput_fields[].kind MUST be exactly one of: raw, clean, derived, quality_flag.\nEach output_fields item MUST include name, kind, expression.\nDo not use synonyms like passthrough/source/base/quality.\nNo wrappers, no extra fields.",
                    failure_errors.join("\n")
                );
                let retry_user = format!(
                    "{}\n\nTarget task_ids:\n{}\n\nSTRICT RETRY REQUIREMENTS:\n{}\n\nReturn schema-valid enrichment JSON.",
                    Self::build_enrichment_prompt_envelope(
                        control_flow::Phase::CleansePlan,
                        crate::data_engineer::prompt_packets::TurnDirective::Verify,
                        crate::data_engineer::plan_kind::PlanKind::Cleanse,
                        &plan.plan_key,
                        planning_context,
                        memo,
                        critique,
                        &crate::data_engineer::plan::summarize_cleanse_plan(plan, 50),
                        &failed,
                        &failure_errors,
                    ),
                    serde_json::to_string_pretty(&failed).unwrap_or_else(|_| "[]".to_string()),
                    retry_hint
                );
                let retry_opts = LlmCallOptions {
                    prompt_id: "data_engineer.cleanse_plan_enrich_retry",
                    ..opts
                };
                let retry_raw = ctx.llm.chat(
                    &[
                        ChatMessage {
                            role: "system".to_string(),
                            content: crate::prompts::plan::cleanse_plan_enrichment_system_prompt(),
                        },
                        ChatMessage {
                            role: "user".to_string(),
                            content: retry_user,
                        },
                    ],
                    &retry_opts,
                )?;
                let retry_enrich = Self::parse_json_typed_lenient::<
                    crate::data_engineer::plan_schema::CleansePlanEnrichmentV1,
                >(&retry_raw)?;
                let (retry_failed, retry_errors) =
                    Self::apply_cleanse_enrichment_items(plan, &failed, retry_enrich.items);
                if !retry_failed.is_empty() {
                    return Err(format!(
                        "cleanse enrichment invalid after bounded retry for task_ids={}: {}",
                        retry_failed.join(","),
                        retry_errors.join(" | ")
                    ));
                }
            }
        }
        let unresolved: Vec<String> = task_ids
            .iter()
            .filter(|task_id| {
                plan.tasks
                    .iter()
                    .find(|t| t.dataset_id.as_str() == task_id.as_str())
                    .map(|t| Self::is_placeholder_cleanse_spec(&t.implementation_spec))
                    .unwrap_or(true)
            })
            .cloned()
            .collect();
        if !unresolved.is_empty() {
            return Err(format!(
                "cleanse enrichment did not produce implementation_spec for task_ids={}",
                unresolved.join(",")
            ));
        }
        Ok(())
    }

    async fn enrich_model_tasks(
        ctx: &AgentCtx,
        planning_context: &str,
        memo: &str,
        critique: &crate::data_engineer::plan_schema::PlanDesignCritiqueV1,
        plan: &mut crate::data_engineer::plan::ModelPlan,
        task_ids: &[String],
    ) -> Result<(), String> {
        use react_core::llm::ChatMessage;
        for chunk in task_ids.chunks(Self::plan_enrich_chunk_size()) {
            let chunk_vec = chunk.to_vec();
            let summary = crate::data_engineer::plan::summarize_model_plan(plan, 50);
            let base_user = format!(
                "{}\n\nTarget task_ids:\n{}\n\nReturn schema-valid enrichment JSON.",
                Self::build_enrichment_prompt_envelope(
                    control_flow::Phase::ModelPlan,
                    crate::data_engineer::prompt_packets::TurnDirective::Compile,
                    crate::data_engineer::plan_kind::PlanKind::Model,
                    &plan.plan_key,
                    planning_context,
                    memo,
                    critique,
                    &summary,
                    &chunk_vec,
                    &[],
                ),
                serde_json::to_string_pretty(&chunk_vec).unwrap_or_else(|_| "[]".to_string())
            );
            let reason_user = format!(
                "Think through the enrichment strategy for these task_ids. Return plain text only, no JSON.\n\n{}",
                Self::build_enrichment_prompt_envelope(
                    control_flow::Phase::ModelPlan,
                    crate::data_engineer::prompt_packets::TurnDirective::Reason,
                    crate::data_engineer::plan_kind::PlanKind::Model,
                    &plan.plan_key,
                    planning_context,
                    memo,
                    critique,
                    &summary,
                    &chunk_vec,
                    &[],
                )
            );
            let reason_memo = ctx.llm.chat(
                &[
                    ChatMessage {
                        role: "system".to_string(),
                        content: crate::prompts::plan::plan_enrichment_reason_system_prompt(),
                    },
                    ChatMessage {
                        role: "user".to_string(),
                        content: reason_user,
                    },
                ],
                &Self::planning_llm_options(
                    PlanningLlmProfile::EnrichmentReason,
                    "data_engineer.model_plan_enrich_reason",
                    ctx.thread_id.clone(),
                ),
            )?;
            let compile_user = Self::compile_prompt_from_reason(&reason_memo, &base_user);
            let mut opts = Self::planning_llm_options(
                PlanningLlmProfile::EnrichmentCompile,
                "data_engineer.model_plan_enrich",
                ctx.thread_id.clone(),
            );
            opts.expected_format = react_core::llm::LlmExpectedFormat::JsonSchemaSpec {
                name: "suite.model_plan_enrichment.v1".to_string(),
                schema: crate::data_engineer::plan_schema::strict_schema_for::<
                    crate::data_engineer::plan_schema::ModelPlanEnrichmentV1,
                >(),
            };
            let raw = ctx.llm.chat(
                &[
                    ChatMessage {
                        role: "system".to_string(),
                        content: crate::prompts::plan::model_plan_enrichment_system_prompt(),
                    },
                    ChatMessage {
                        role: "user".to_string(),
                        content: compile_user,
                    },
                ],
                &opts,
            )?;
            let enrich = Self::parse_json_typed_lenient::<
                crate::data_engineer::plan_schema::ModelPlanEnrichmentV1,
            >(&raw)?;
            let (failed, failure_errors) =
                Self::apply_model_enrichment_items(plan, &chunk_vec, enrich.items);
            if !failed.is_empty() {
                let retry_hint = format!(
                    "You previously returned invalid implementation_spec.\nErrors:\n{}\nOnly emit implementation_spec object with keys: spec_version,grain,inputs,joins,metrics,output_fields,assumptions.\noutput_fields[].kind MUST be exactly one of: raw, clean, derived, quality_flag.\nEach output_fields item MUST include name, kind, expression.\nDo not use synonyms like passthrough/source/base/quality.\nNo wrappers, no extra fields.",
                    failure_errors.join("\n")
                );
                let retry_user = format!(
                    "{}\n\nTarget task_ids:\n{}\n\nSTRICT RETRY REQUIREMENTS:\n{}\n\nReturn schema-valid enrichment JSON.",
                    Self::build_enrichment_prompt_envelope(
                        control_flow::Phase::ModelPlan,
                        crate::data_engineer::prompt_packets::TurnDirective::Verify,
                        crate::data_engineer::plan_kind::PlanKind::Model,
                        &plan.plan_key,
                        planning_context,
                        memo,
                        critique,
                        &crate::data_engineer::plan::summarize_model_plan(plan, 50),
                        &failed,
                        &failure_errors,
                    ),
                    serde_json::to_string_pretty(&failed).unwrap_or_else(|_| "[]".to_string()),
                    retry_hint
                );
                let retry_opts = LlmCallOptions {
                    prompt_id: "data_engineer.model_plan_enrich_retry",
                    ..opts
                };
                let retry_raw = ctx.llm.chat(
                    &[
                        ChatMessage {
                            role: "system".to_string(),
                            content: crate::prompts::plan::model_plan_enrichment_system_prompt(),
                        },
                        ChatMessage {
                            role: "user".to_string(),
                            content: retry_user,
                        },
                    ],
                    &retry_opts,
                )?;
                let retry_enrich = Self::parse_json_typed_lenient::<
                    crate::data_engineer::plan_schema::ModelPlanEnrichmentV1,
                >(&retry_raw)?;
                let (retry_failed, retry_errors) =
                    Self::apply_model_enrichment_items(plan, &failed, retry_enrich.items);
                if !retry_failed.is_empty() {
                    return Err(format!(
                        "model enrichment invalid after bounded retry for task_ids={}: {}",
                        retry_failed.join(","),
                        retry_errors.join(" | ")
                    ));
                }
            }
        }
        let unresolved: Vec<String> = task_ids
            .iter()
            .filter(|task_id| {
                plan.tasks
                    .iter()
                    .find(|t| t.name.as_str() == task_id.as_str())
                    .map(|t| Self::is_placeholder_model_spec(&t.implementation_spec))
                    .unwrap_or(true)
            })
            .cloned()
            .collect();
        if !unresolved.is_empty() {
            return Err(format!(
                "model enrichment did not produce implementation_spec for task_ids={}",
                unresolved.join(",")
            ));
        }
        Ok(())
    }

    fn collect_targeted_semantic_tasks(
        issues: &[crate::data_engineer::plan::PlanSemanticIssue],
        candidates: &[String],
    ) -> Vec<String> {
        let mut out = Vec::new();
        for issue in issues {
            if let Some(task_id) = issue.task_id.as_ref() {
                let key = task_id.trim();
                if !key.is_empty() && candidates.iter().any(|c| c == key) {
                    out.push(key.to_string());
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }

    async fn set_pending_patch_plan_intent(
        thread_store: &ThreadStore,
        thread_id: &str,
        phase: control_flow::Phase,
        entry_plan_key: Option<String>,
        entry_plan_digest: Option<String>,
    ) -> Result<(), String> {
        crate::data_engineer::state_manager::mutate_execution_state(thread_store, thread_id, |es| {
            es.set_pending_patch_plan_intent(phase, entry_plan_key.clone(), entry_plan_digest.clone());
        })
        .await
        .map(|_| ())
    }

    async fn set_pending_patch_impl_intent(
        thread_store: &ThreadStore,
        thread_id: &str,
        phase: control_flow::Phase,
    ) -> Result<(), String> {
        crate::data_engineer::state_manager::mutate_execution_state(thread_store, thread_id, |es| {
            es.set_pending_patch_impl_intent(phase);
        })
        .await
        .map(|_| ())
    }

    async fn clear_pending_loopback_intent(
        thread_store: &ThreadStore,
        thread_id: &str,
    ) -> Result<(), String> {
        crate::data_engineer::state_manager::mutate_execution_state(thread_store, thread_id, |es| {
            es.clear_pending_loopback_intent();
        })
        .await
        .map(|_| ())
    }

    fn patch_plan_intent_blocks_fast_forward(
        execution_state: &crate::data_engineer::progress_controller::ExecutionState,
        phase: control_flow::Phase,
        current_plan_key: &str,
        current_plan_digest: Option<&str>,
    ) -> bool {
        crate::data_engineer::phase_gate::patch_plan_intent_blocks_fast_forward(
            execution_state,
            phase,
            current_plan_key,
            current_plan_digest,
        )
    }

    fn patch_impl_intent_unsatisfied(
        execution_state: &crate::data_engineer::progress_controller::ExecutionState,
        phase: control_flow::Phase,
    ) -> bool {
        crate::data_engineer::phase_gate::patch_impl_intent_unsatisfied(execution_state, phase)
    }

    fn derive_single_target_repair_path(
        execution_state: &crate::data_engineer::progress_controller::ExecutionState,
        last_validate_failed_models: &[crate::data_engineer::progress_controller::FailedModelRef],
    ) -> Option<String> {
        crate::data_engineer::phase_gate::derive_single_target_repair_path(
            execution_state,
            last_validate_failed_models,
        )
    }

    async fn approve_cleanse_plan_draft_and_advance(
        thread_store: &ThreadStore,
        thread_id: &str,
        phase: control_flow::Phase,
        actx: &AgentCtx,
        log_len: usize,
        transition_reason_code: PhaseReasonCode,
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
            crate::data_engineer::plan::save_cleanse_plan(actx, &p)
                .await
                .map_err(|e| format!("failed to persist pruned-empty cleanse plan: {e}"))?;
            // Stay in plan phase; the next iteration will generate a new plan.
            apply_phase_transition(
                thread_store,
                thread_id,
                Some(phase),
                phase,
                control_flow::TransitionIntent::Annotation,
                Some(PhaseReasonCode::PlanPrunedEmpty),
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
            crate::data_engineer::plan::save_cleanse_plan(actx, &p)
                .await
                .map_err(|e| format!("failed to persist semantically-invalid cleanse plan: {e}"))?;
            apply_phase_transition(
                thread_store,
                thread_id,
                Some(phase),
                phase,
                control_flow::TransitionIntent::Annotation,
                Some(PhaseReasonCode::PlanSemanticInvalid),
                Some(serde_json::json!({ "plan_key": p.plan_key, "errors": v.errors })),
            )
            .await?;
            return Ok(true);
        }

        p.status = crate::data_engineer::plan::PlanStatus::Approved;
        // Scope progress to *this* plan instance so old tool calls can't auto-complete a newly approved plan.
        p.progress.last_applied_step_idx = log_len;
        crate::data_engineer::plan::save_cleanse_plan(actx, &p)
            .await
            .map_err(|e| format!("failed to persist approved cleanse plan: {e}"))?;
        let _ = Self::clear_pending_loopback_intent(thread_store, thread_id).await;

        apply_phase_transition(
            thread_store,
            thread_id,
            Some(phase),
            control_flow::Phase::CleanseAuthor,
            control_flow::TransitionIntent::Forward,
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
        transition_reason_code: PhaseReasonCode,
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
            crate::data_engineer::plan::save_model_plan(actx, &p)
                .await
                .map_err(|e| format!("failed to persist pruned-empty model plan: {e}"))?;
            // Stay in plan phase; the next iteration will generate a new plan.
            apply_phase_transition(
                thread_store,
                thread_id,
                Some(phase),
                phase,
                control_flow::TransitionIntent::Annotation,
                Some(PhaseReasonCode::PlanPrunedEmpty),
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
            crate::data_engineer::plan::save_model_plan(actx, &p)
                .await
                .map_err(|e| format!("failed to persist semantically-invalid model plan: {e}"))?;
            apply_phase_transition(
                thread_store,
                thread_id,
                Some(phase),
                phase,
                control_flow::TransitionIntent::Annotation,
                Some(PhaseReasonCode::PlanSemanticInvalid),
                Some(serde_json::json!({ "plan_key": p.plan_key, "errors": v.errors })),
            )
            .await?;
            return Ok(true);
        }

        p.status = crate::data_engineer::plan::PlanStatus::Approved;
        p.progress.last_applied_step_idx = log_len;
        crate::data_engineer::plan::save_model_plan(actx, &p)
            .await
            .map_err(|e| format!("failed to persist approved model plan: {e}"))?;
        let _ = Self::clear_pending_loopback_intent(thread_store, thread_id).await;

        apply_phase_transition(
            thread_store,
            thread_id,
            Some(phase),
            control_flow::Phase::ModelAuthor,
            control_flow::TransitionIntent::Forward,
            Some(transition_reason_code),
            Some(transition_reason_detail),
        )
        .await?;
        Ok(true)
    }

    async fn authoring_complete_reason_detail(
        thread_store: &ThreadStore,
        thread_id: &str,
        has_proj: bool,
        has_models: bool,
    ) -> serde_json::Value {
        let execution_state = crate::data_engineer::progress_controller::ExecutionState::load(
            thread_store,
            thread_id,
        )
        .await
        .unwrap_or_else(crate::data_engineer::progress_controller::ExecutionState::new);
        let guard = control_flow::derive_guard_state_from_execution_state(&execution_state);
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
                if Self::normalize_string_vec(&pt.invariants)
                    != Self::normalize_string_vec(&t.invariants)
                {
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
                if Self::normalize_string_vec(&pt.invariants)
                    != Self::normalize_string_vec(&t.invariants)
                {
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

    fn build_review_question_with_context(
        question: &str,
        phase: control_flow::Phase,
        execution_state: &crate::data_engineer::progress_controller::ExecutionState,
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

        let entry_reason_code: Option<PhaseReasonCode> = execution_state.phase_reason_code;
        let entry_reason_detail: serde_json::Value = execution_state
            .phase_reason_detail
            .clone()
            .unwrap_or(serde_json::Value::Null);

        let mut prior_review_block: Option<String> = None;
        if matches!(
            entry_reason_code,
            Some(
                PhaseReasonCode::ReviewProceed
                    | PhaseReasonCode::ReviewPatchPlan
                    | PhaseReasonCode::ReviewPatchImpl
            )
        ) {
            let rd = entry_reason_detail.clone();
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
        }

        let mut ctx_lines: Vec<String> = Vec::new();
        if let Some(prior) = prior_review_block {
            ctx_lines.push(prior);
        }
        if let Some(last_mutation) = execution_state.last_mutation_summary.as_ref() {
            ctx_lines.push(format!(
                "Most recent mutation summary (state-derived):\n{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "op": last_mutation.op,
                    "affected_paths": last_mutation.affected_paths,
                    "select_terms": last_mutation.select_terms,
                }))
                .unwrap_or_else(|_| "{}".to_string())
            ));
        }
        if entry_reason_code.is_some() || !entry_reason_detail.is_null() {
            ctx_lines.push(format!(
                "Why we are reviewing now:\n- entry_reason_code: {}\n- entry_reason_detail: {}",
                entry_reason_code.map(|rc| rc.as_str()).unwrap_or("null"),
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

    fn validate_agent_type(agent_type: &str) -> Result<AgentMode, String> {
        AgentMode::parse(agent_type)
    }

    fn inject_review_question(question: &str) -> String {
        crate::prompts::shared::user_goal_line("Review request:", question)
    }

    fn inject_model_question(question: &str) -> String {
        crate::prompts::shared::user_goal_line("Modeling goal:", question)
    }

    fn inject_cleanse_question(question: &str) -> String {
        crate::prompts::shared::user_goal_line("Cleansing goal:", question)
    }

    fn build_tools(agent_mode: AgentMode, sctx: &SuiteCtx) -> Result<ToolRegistry, String> {
        use crate::data_engineer::tools::{
            artifacts::ArtifactsTool, files_tool::FilesTool, sql_run::SqlRunTool,
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
                    let guard = crate::data_engineer::progress_controller::ExecutionState::load(
                        store, tid,
                    )
                    .await
                    .map(|st| {
                        crate::data_engineer::control_flow::derive_guard_state_from_execution_state(
                            &st,
                        )
                    })
                    .unwrap_or_default();
                    if guard.last_validate_failed && !guard.mutated_since_fail {
                        return Err(crate::data_engineer::controller_kernel::guard_block_error(
                            crate::data_engineer::controller_kernel::GuardReason::MutationRequiredAfterValidateFailure,
                        ));
                    }
                    if runtime_validate && guard.probe_required && !guard.probe_satisfied {
                        return Err(crate::data_engineer::controller_kernel::guard_block_error(
                            crate::data_engineer::controller_kernel::GuardReason::ProbeRequiredAfterRuntimeFailure,
                        ));
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

        let allow_user_interrupt_tools = !Self::headless_mode_enabled();
        let caps = Self::agent_capability_profile(agent_mode, allow_user_interrupt_tools);

        // Allow review to read the current dbt project state (manifest/schema/models)
        // without permitting writes.
        struct ReadOnlyFilesTool {
            inner: FilesTool,
        }
        #[async_trait::async_trait]
        impl react_core::tools::Tool for ReadOnlyFilesTool {
            fn name(&self) -> &'static str {
                "file"
            }
            async fn call(
                &self,
                args: serde_json::Value,
                ctx: &react_core::agent::AgentCtx,
            ) -> Result<serde_json::Value, String> {
                if !crate::data_engineer::tool_ops::is_file_read_op(&args) {
                    return Err(
                        "file is read-only for review; use op='get' or op='list' (mutating ops are disabled: patch/rm/mv)".to_string(),
                    );
                }
                self.inner.call(args, ctx).await
            }
        }

        if caps.contains(&AgentToolCapability::ReadOnlyFile) {
            registry.register(ReadOnlyFilesTool {
                inner: FilesTool {
                    datasets: sctx.datasets.clone(),
                },
            });
        } else if caps.contains(&AgentToolCapability::MutableFile) {
            registry.register(FilesTool {
                datasets: sctx.datasets.clone(),
            });
        }
        if caps.contains(&AgentToolCapability::RunSql) {
            registry.register(SqlRunTool {
                query: query.clone(),
            });
        }
        if caps.contains(&AgentToolCapability::AskUser) {
            registry.register(tools::ask_user::AskUserTool);
        }
        if caps.contains(&AgentToolCapability::AskApproval) {
            registry.register(tools::ask_approval::AskApprovalTool);
        }
        if caps.contains(&AgentToolCapability::SearchDbtExamples) {
            registry.register(tools::dbt_examples::SearchDbtExamplesTool);
        }
        if caps.contains(&AgentToolCapability::StagingModel) {
            registry.register(tools::staging_model::StagingModelTool {
                datasets: sctx.datasets.clone(),
            });
        }
        if caps.contains(&AgentToolCapability::GoldModel) {
            registry.register(tools::gold_model::GoldModelTool);
        }
        if caps.contains(&AgentToolCapability::DbtValidate) {
            registry.register(ThreadDerivedDbtValidateTool {
                inner: tools::dbt_validate::DbtValidateTool {
                    datasets: sctx.datasets.clone(),
                    catalog: sctx.catalog.clone(),
                },
            });
        }
        if caps.contains(&AgentToolCapability::PublishDbt) {
            registry.register(tools::publish_dbt_to_provider::PublishDbtToProviderTool {
                datasets: sctx.datasets.clone(),
                catalog: sctx.catalog.clone(),
            });
        }
        if caps.contains(&AgentToolCapability::SqlRegister) {
            registry.register(tools::sql_register::SqlRegisterTool);
        }
        if caps.contains(&AgentToolCapability::CatalogNote) {
            registry.register(tools::catalog_note::CatalogNoteTool);
        }
        if caps.contains(&AgentToolCapability::Artifacts) {
            registry.register(ArtifactsTool);
        }

        Ok(registry)
    }

    fn build_tools_card_for_agent_type(agent_mode: AgentMode) -> String {
        let allow_user_interrupt_tools = !Self::headless_mode_enabled();
        let caps = Self::agent_capability_profile(agent_mode, allow_user_interrupt_tools);
        match agent_mode {
            AgentMode::Review => Self::build_tools_card(
                "Allowed tools (review mode, read-only):",
                vec![
                    "- file(args:{op:\"list\"|\"get\", prefix?:string, path?:string, limit?:int, max_chars?:int})".to_string(),
                    "- sql_schema / sql_stats / sql_sample / vect_query (read-only context)".to_string(),
                    "- artifacts".to_string(),
                ],
                Vec::new(),
                Some(
                    "Not available: run_sql, staging_model, gold_model, file patch/rm/mv, dbt_validate, publish_dbt_to_provider."
                        .to_string(),
                ),
            ),
            AgentMode::Ask => {
                let mut lines = vec!["- file(args:{op:\"list\"|\"get\"|\"patch\"|\"rm\"|\"mv\", ...})".to_string(),
                    "- sql_schema / sql_stats / sql_sample / vect_query (discovery context)".to_string()];
                if caps.contains(&AgentToolCapability::RunSql) {
                    lines.push("- run_sql(args:{sql:string})".to_string());
                }
                if caps.contains(&AgentToolCapability::AskApproval) {
                    lines.push("- ask_approval(args:{prompt:string})".to_string());
                }
                if caps.contains(&AgentToolCapability::Artifacts) {
                    lines.push("- artifacts".to_string());
                }
                if caps.contains(&AgentToolCapability::AskUser) {
                    lines.push("- ask_user(args:{prompt:string})".to_string());
                }
                Self::build_tools_card("Allowed tools (ask mode):", lines, Vec::new(), None)
            }
            AgentMode::Cleanse => {
                let mut lines = vec!["- file(args:{op:\"list\"|\"get\"|\"patch\"|\"rm\"|\"mv\", ...})".to_string(),
                    "- sql_schema / sql_stats / sql_sample / vect_query (discovery context)".to_string()];
                if caps.contains(&AgentToolCapability::RunSql) {
                    lines.push("- run_sql(args:{sql:string})".to_string());
                }
                if caps.contains(&AgentToolCapability::StagingModel) {
                    lines.push("- staging_model(args:{dataset_ids:[string], instructions?:string, sql?:string|staging_model?:string|expression?:string})".to_string());
                }
                if caps.contains(&AgentToolCapability::DbtValidate)
                    || caps.contains(&AgentToolCapability::PublishDbt)
                {
                    lines.push("- dbt_validate / publish_dbt_to_provider".to_string());
                }
                if caps.contains(&AgentToolCapability::AskApproval) {
                    lines.push("- ask_approval(args:{prompt:string})".to_string());
                }
                if caps.contains(&AgentToolCapability::Artifacts) {
                    lines.push("- artifacts".to_string());
                }
                if caps.contains(&AgentToolCapability::AskUser) {
                    lines.push("- ask_user(args:{prompt:string})".to_string());
                }
                Self::build_tools_card("Allowed tools (cleanse mode):", lines, Vec::new(), None)
            }
            AgentMode::Model | AgentMode::Agent => {
                let mut lines = vec!["- file(args:{op:\"list\"|\"get\"|\"patch\"|\"rm\"|\"mv\", ...})".to_string(),
                    "- sql_schema / sql_stats / sql_sample / vect_query (discovery context)".to_string()];
                if caps.contains(&AgentToolCapability::RunSql) {
                    lines.push("- run_sql(args:{sql:string})".to_string());
                }
                if caps.contains(&AgentToolCapability::StagingModel)
                    || caps.contains(&AgentToolCapability::GoldModel)
                {
                    lines.push("- staging_model / gold_model".to_string());
                }
                if caps.contains(&AgentToolCapability::DbtValidate)
                    || caps.contains(&AgentToolCapability::PublishDbt)
                {
                    lines.push("- dbt_validate / publish_dbt_to_provider".to_string());
                }
                if caps.contains(&AgentToolCapability::AskApproval) {
                    lines.push("- ask_approval(args:{prompt:string})".to_string());
                }
                if caps.contains(&AgentToolCapability::Artifacts) {
                    lines.push("- artifacts".to_string());
                }
                if caps.contains(&AgentToolCapability::AskUser) {
                    lines.push("- ask_user(args:{prompt:string})".to_string());
                }
                Self::build_tools_card("Allowed tools (model mode):", lines, Vec::new(), None)
            }
        }
    }

    fn build_tools_for_phase(
        phase: control_flow::Phase,
        guard: &control_flow::DerivedGuardState,
        _allow_ask_approval: bool,
        sctx: &SuiteCtx,
        allowed_batch: Option<AllowedBatch>,
        single_target_repair_path: Option<String>,
        suppress_manifest_json_in_plan: bool,
    ) -> Result<(ToolRegistry, String), String> {
        use crate::data_engineer::tools::{
            artifacts::ArtifactsTool, files_tool::FilesTool, json_file::JsonFileTool,
            sql_run::SqlRunTool, sql_sample::SqlSampleTool, sql_schema::SqlSchemaTool,
            sql_stats::SqlStatsTool, vect_query::VectQueryTool,
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

        let tools_card: String;

        match phase {
            control_flow::Phase::CleansePlan | control_flow::Phase::ModelPlan => {
                // Plan phases: read-only discovery + (optional) probes. No dbt file mutations.
                let suppress_manifest_json =
                    suppress_manifest_json_in_plan && phase == control_flow::Phase::ModelPlan;
                reg.register(SqlRunTool {
                    query: query.clone(),
                });
                reg.register(tools::dbt_examples::SearchDbtExamplesTool);

                // Read-only file tool (no patch).
                struct ReadOnlyFilesTool {
                    inner: FilesTool,
                }
                #[async_trait::async_trait]
                impl react_core::tools::Tool for ReadOnlyFilesTool {
                    fn name(&self) -> &'static str {
                        "file"
                    }
                    async fn call(
                        &self,
                        args: serde_json::Value,
                        ctx: &react_core::agent::AgentCtx,
                    ) -> Result<serde_json::Value, String> {
                        if !crate::data_engineer::tool_ops::is_file_read_op(&args) {
                            return Err(
                                "file is read-only in plan phases; use op='get' or op='list' (mutating ops are disabled: patch/rm/mv)".to_string(),
                            );
                        }
                        self.inner.call(args, ctx).await
                    }
                }
                reg.register(ReadOnlyFilesTool {
                    inner: FilesTool {
                        datasets: sctx.datasets.clone(),
                    },
                });
                if !suppress_manifest_json {
                    reg.register(JsonFileTool);
                }

                let mut tool_lines: Vec<String> = vec![
                    "- file(args:{op:\"list\", prefix?:string, limit?:int} | {op:\"get\", path:string, max_chars?:int})".to_string(),
                    "- sql_schema / sql_stats / sql_sample / vect_query (discovery context)".to_string(),
                    "- run_sql (targeted probes)".to_string(),
                    "- artifacts".to_string(),
                ];
                if suppress_manifest_json {
                    tool_lines.push("- json_file is temporarily disabled for this model_plan retry due to repeated manifest lookup failures; use deterministic fallback evidence (file + sql_schema + sql_stats/sql_sample).".to_string());
                } else {
                    tool_lines.push("- json_file(args:{op:\"get_item\", path:string, pointer?:string} | {op:\"query\", path:string, pointer?:string, unique_id?:string, name?:string, resource_type?:string, limit?:int})".to_string());
                    tool_lines.push("  - IMPORTANT: use args.op (NOT args.type). For list use args.prefix (NOT path:\".\").".to_string());
                    tool_lines.push("  - For manifest queries use canonical path: target/manifest.json (NOT manifest.json).".to_string());
                }
                tools_card = Self::build_tools_card(
                    "Allowed tools (plan phase, read-only):",
                    tool_lines,
                    Vec::new(),
                    Some(
                        "Not available: staging_model, gold_model, file patch/rm/mv, dbt_validate, publish_dbt_to_provider.".to_string(),
                    ),
                );
            }
            control_flow::Phase::CleanseAuthor | control_flow::Phase::ModelAuthor => {
                // Authoring phases: allow investigation + mutations; validation/publish are suite-driven.
                //
                // Deterministic repair hard-cutover:
                // - classic gate: after validate fail with no mutation yet, require mutation next.
                // - single-target repair mode: ALWAYS require mutation next, even if a prior mutation
                //   already happened in this repair cycle (prevents read/get loops against removed targets).
                let hard_mutation_only = (guard.last_validate_failed && !guard.mutated_since_fail)
                    || single_target_repair_path.is_some();
                let allow_probe_sql = guard.probe_required && !guard.probe_satisfied;
                let plan_batched_cleanse_sql = phase == control_flow::Phase::CleanseAuthor
                    && matches!(allowed_batch, Some(AllowedBatch::CleanseSqlDatasetIds(_)));
                let plan_batched_cleanse_schema = phase == control_flow::Phase::CleanseAuthor
                    && matches!(allowed_batch, Some(AllowedBatch::CleanseSchemaDatasetIds(_)));
                let plan_batched_model_sql = phase == control_flow::Phase::ModelAuthor
                    && matches!(allowed_batch, Some(AllowedBatch::ModelSqlItemNames(_)));
                let plan_batched_model_schema = phase == control_flow::Phase::ModelAuthor
                    && matches!(allowed_batch, Some(AllowedBatch::ModelSchemaItemNames(_)));
                let authoring_policy = crate::data_engineer::authoring_driver::derive_authoring_tool_policy(
                    crate::data_engineer::authoring_driver::AuthoringToolPolicyInput {
                        hard_mutation_only,
                        single_target_repair: single_target_repair_path.is_some(),
                        allow_probe_sql,
                        plan_batched_cleanse_sql,
                        plan_batched_cleanse_schema,
                        plan_batched_model_sql,
                        plan_batched_model_schema,
                    },
                );

                if hard_mutation_only {
                    // Mutation-only file tool to avoid "read-only thrash" when we require a mutation next.
                    struct PutOnlyFilesTool {
                        inner: FilesTool,
                        single_target_path: Option<String>,
                    }
                    #[async_trait::async_trait]
                    impl react_core::tools::Tool for PutOnlyFilesTool {
                        fn name(&self) -> &'static str {
                            "file"
                        }
                        async fn call(
                            &self,
                            args: serde_json::Value,
                            ctx: &react_core::agent::AgentCtx,
                        ) -> Result<serde_json::Value, String> {
                            let op = args.get("op").and_then(|x| x.as_str()).unwrap_or("get");
                            let is_repair_mutation =
                                crate::data_engineer::tool_ops::is_file_repair_mutation_op(&args);
                            if self.single_target_path.is_some() && !is_repair_mutation {
                                return Err("file is in deterministic single-target repair mode; only op='patch'|'rm'|'mv' is allowed.".to_string());
                            }
                            if self.single_target_path.is_none()
                                && !is_repair_mutation
                            {
                                return Err("file is mutation-only right now (a mutating fix is required before any further validation). Allowed ops: patch/rm/mv.".to_string());
                            }
                            if let Some(want) = self.single_target_path.as_ref() {
                                fn collect_paths(v: &serde_json::Value) -> Vec<String> {
                                    let mut out: Vec<String> = Vec::new();
                                    if let Some(p) = v.get("path").and_then(|x| x.as_str()) {
                                        let p = p.trim();
                                        if !p.is_empty() {
                                            out.push(p.to_string());
                                        }
                                    }
                                    if let Some(p) = v.get("from").and_then(|x| x.as_str()) {
                                        let p = p.trim();
                                        if !p.is_empty() {
                                            out.push(p.to_string());
                                        }
                                    }
                                    out
                                }
                                let mut paths = collect_paths(&args);
                                paths.sort();
                                paths.dedup();
                                if paths.is_empty() {
                                    return Err(format!(
                                        "file deterministic repair mode requires explicit path/from='{}'.",
                                        want
                                    ));
                                }
                                if paths.len() != 1 || paths[0] != *want {
                                    return Err(format!(
                                        "file deterministic single-target repair mode violation: only '{}' may be mutated right now (got: {}).",
                                        want,
                                        paths.join(", ")
                                    ));
                                }
                            }
                            // Deterministic repair ladder enforcement (hard cutover).
                            if let (Some(store), Some(thread_id), Some(want)) = (
                                ctx.thread_store.as_ref(),
                                ctx.thread_id.as_deref(),
                                self.single_target_path.as_ref(),
                            ) {
                                if is_repair_mutation {
                                    let es = crate::data_engineer::progress_controller::ExecutionState::load(
                                        store, thread_id,
                                    )
                                    .await
                                    .unwrap_or_else(
                                        crate::data_engineer::progress_controller::ExecutionState::new,
                                    );

                                    match es.ladder_step {
                                        crate::data_engineer::progress_controller::RepairLadderStep::Stop => {
                                            return Err(format!(
                                                "deterministic repair ladder stop: '{}' did not converge after prior repair attempts. Stop and apply a manual fix for '{}' before re-running.",
                                                want, want
                                            ));
                                        }
                                        crate::data_engineer::progress_controller::RepairLadderStep::ReplaceFile => {
                                            if op == "patch" {
                                                // Hard cutover: Cursor/Aider hunks-only patches only.
                                                let patch_text = args
                                                    .get("patch_text")
                                                    .and_then(|v| v.as_str())
                                                    .unwrap_or("");
                                                let has_patch_text = !patch_text.trim().is_empty()
                                                    && patch_text.trim_start().starts_with("@@");
                                                let guard_path_ok = args
                                                    .get("path")
                                                    .and_then(|v| v.as_str())
                                                    .map(|p| p.trim() == want)
                                                    .unwrap_or(false);
                                                if patch_text.contains("@@ ... @@") {
                                                    return Err(format!(
                                                        "deterministic repair ladder step for '{}': placeholder hunk header '@@ ... @@' is not allowed. Use real hunks with exact context lines from the current file content.",
                                                        want
                                                    ));
                                                }
                                                if !has_patch_text || !guard_path_ok {
                                                    return Err(format!(
                                                        "deterministic repair ladder step requires a guarded single-file patch for '{}': args must include path='{}' + patch_text starting with '@@' (Cursor/Aider hunks-only; no ---/+++ headers).",
                                                        want,
                                                        want
                                                    ));
                                                }
                                            }
                                        }
                                        crate::data_engineer::progress_controller::RepairLadderStep::PatchTarget => {}
                                    }
                                }
                            }

                            let res = self.inner.call(args.clone(), ctx).await;

                            // Update repair state after the attempt (best-effort, but should fail fast if persistence breaks).
                            if let (Some(store), Some(thread_id), Some(want)) = (
                                ctx.thread_store.as_ref(),
                                ctx.thread_id.as_deref(),
                                self.single_target_path.as_ref(),
                            ) {
                                if is_repair_mutation {
                                    let mut es =
                                        crate::data_engineer::progress_controller::ExecutionState::load(
                                            store, thread_id,
                                        )
                                        .await
                                        .unwrap_or_else(
                                            crate::data_engineer::progress_controller::ExecutionState::new,
                                        );
                                    if es.target_path.as_deref().unwrap_or("").trim().is_empty() {
                                        es.target_path = Some(want.clone());
                                    }

                                    match &res {
                                        Ok(v) => {
                                            let ok = v
                                                .get("ok")
                                                .and_then(|x| x.as_bool())
                                                .unwrap_or(false);
                                            let mutated = v
                                                .get("mutated")
                                                .and_then(|x| x.as_bool())
                                                .unwrap_or(false);
                                            es.note_patch_attempt(ok, mutated);
                                        }
                                        Err(_) => {
                                            es.note_patch_attempt(false, false);
                                        }
                                    }
                                    // Persist state; hard fail if we cannot persist during repair mode.
                                    es.save(store, thread_id).await?;
                                }
                            }

                            res
                        }
                    }
                    reg.register(PutOnlyFilesTool {
                        inner: FilesTool {
                            datasets: sctx.datasets.clone(),
                        },
                        single_target_path: single_target_repair_path.clone(),
                    });

                    // Keep targeted probes available: probe requirements can be asserted after runtime failures,
                    // and those probes must be satisfiable even when the next step must be a mutation.
                    struct ProbeAwareRunSqlTool {
                        inner: SqlRunTool,
                    }
                    #[async_trait::async_trait]
                    impl react_core::tools::Tool for ProbeAwareRunSqlTool {
                        fn name(&self) -> &'static str {
                            "run_sql"
                        }
                        async fn call(
                            &self,
                            args: serde_json::Value,
                            ctx: &react_core::agent::AgentCtx,
                        ) -> Result<serde_json::Value, String> {
                            let sql = args
                                .get("sql")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            if let (Some(store), Some(thread_id)) =
                                (ctx.thread_store.as_ref(), ctx.thread_id.as_deref())
                            {
                                let mut es = crate::data_engineer::progress_controller::ExecutionState::load(
                                    store, thread_id,
                                )
                                .await
                                .unwrap_or_else(
                                    crate::data_engineer::progress_controller::ExecutionState::new,
                                );
                                if matches!(
                                    es.probe_requirement_status(),
                                    crate::data_engineer::progress_controller::ProbeRequirementStatus::ExhaustedRequireMutation
                                ) {
                                    return Err("run_sql probe loop exhausted for this validate-failure cycle; apply a mutating file fix before probing again.".to_string());
                                }
                                let res = self.inner.call(args, ctx).await;
                                if es.last_validate_ok == Some(false) && es.hard_mutation_repair_mode {
                                    match &res {
                                        Ok(v) => {
                                            let ok =
                                                v.get("ok").and_then(|x| x.as_bool()).unwrap_or(false);
                                            let sig = crate::data_engineer::progress_controller::ProbeSignature::from_run_sql(
                                                &sql, v,
                                            );
                                            let _ = es.note_probe_attempt(&sql, ok, sig);
                                        }
                                        Err(_) => {
                                            let sig = crate::data_engineer::progress_controller::ProbeSignature::from_run_sql(
                                                &sql,
                                                &serde_json::json!({}),
                                            );
                                            let _ = es.note_probe_attempt(&sql, false, sig);
                                        }
                                    }
                                    es.save(store, thread_id).await?;
                                }
                                return res;
                            }
                            self.inner.call(args, ctx).await
                        }
                    }
                    if allow_probe_sql {
                        reg.register(ProbeAwareRunSqlTool {
                            inner: SqlRunTool {
                                query: query.clone(),
                            },
                        });
                    }

                    // In hard_mutation_only mode, only expose schema-batch tools when we are in
                    // schema repair mode (single_target_repair_path is absent). SQL-target repair
                    // mode must stay file-targeted to avoid schema-tool no-op loops.
                    let mut tool_lines: Vec<String> = Vec::new();
                    if phase == control_flow::Phase::CleanseAuthor
                        && !matches!(
                            authoring_policy,
                            crate::data_engineer::authoring_driver::AuthoringToolPolicy::HardMutationSingleTarget
                        )
                    {
                        reg.register(
                            tools::apply_next_schema_batch::ApplyNextCleanseSchemaBatchTool {
                                datasets: sctx.datasets.clone(),
                            },
                        );
                        tool_lines.push(
                            "- apply_next_cleanse_schema_batch(args:{instructions?:string})"
                                .to_string(),
                        );
                    }
                    if phase == control_flow::Phase::ModelAuthor
                        && !matches!(
                            authoring_policy,
                            crate::data_engineer::authoring_driver::AuthoringToolPolicy::HardMutationSingleTarget
                        )
                    {
                        reg.register(
                            tools::apply_next_schema_batch::ApplyNextModelSchemaBatchTool {
                                datasets: sctx.datasets.clone(),
                            },
                        );
                        tool_lines.push(
                            "- apply_next_model_schema_batch(args:{instructions?:string})"
                                .to_string(),
                        );
                    }
                    tool_lines.extend_from_slice(&[
                        "- file(args:{op:\"patch\"|\"rm\"|\"mv\", ...})".to_string(),
                        "  - op=patch args: {path:string, patch_text:string} (Cursor/Aider hunks-only; patch_text starts with '@@' and MUST NOT include ---/+++ or diff --git)".to_string(),
                        "  - op=rm args: {path:string, expected_sha256?:string}".to_string(),
                        "  - op=mv args: {from:string, to:string, expected_sha256?:string}".to_string(),
                    ]);
                    if allow_probe_sql {
                        tool_lines.push("- run_sql(args:{sql:string}) (targeted probes are currently required by probe gate)".to_string());
                    }
                    if single_target_repair_path.is_some() {
                        tool_lines.push("Deterministic single-target repair mode is active: only file op=patch/rm/mv targeting the current failing model file is allowed.".to_string());
                    }
                    tools_card = Self::build_tools_card(
                        "Allowed tools (authoring phase; HARD constraint: mutation required next):",
                        tool_lines,
                        Vec::new(),
                        Some(
                            "Not available: read/explore tools, dbt_validate, publish_dbt_to_provider."
                                .to_string(),
                        ),
                    );
                } else {
                    // Normal authoring: allow read/explore + probes.
                    if phase == control_flow::Phase::CleanseAuthor {
                        if let Some(ab) = allowed_batch.clone() {
                            match ab {
                                AllowedBatch::CleanseSqlDatasetIds(_) => {
                                    reg.register(tools::apply_next_batch::ApplyNextCleanseBatchTool {
                                        datasets: sctx.datasets.clone(),
                                    });
                                }
                                AllowedBatch::CleanseSchemaDatasetIds(_) => {
                                    reg.register(
                                        tools::apply_next_schema_batch::ApplyNextCleanseSchemaBatchTool {
                                            datasets: sctx.datasets.clone(),
                                        },
                                    );
                                }
                                _ => {}
                            }
                        } else {
                            reg.register(tools::staging_model::StagingModelTool {
                                datasets: sctx.datasets.clone(),
                            });
                            reg.register(
                                tools::apply_next_schema_batch::ApplyNextCleanseSchemaBatchTool {
                                    datasets: sctx.datasets.clone(),
                                },
                            );
                        }
                    }
                    if phase == control_flow::Phase::ModelAuthor {
                        if let Some(ab) = allowed_batch.clone() {
                            match ab {
                                AllowedBatch::ModelSqlItemNames(_) => {
                                    reg.register(tools::apply_next_batch::ApplyNextModelBatchTool);
                                }
                                AllowedBatch::ModelSchemaItemNames(_) => {
                                    reg.register(
                                        tools::apply_next_schema_batch::ApplyNextModelSchemaBatchTool {
                                            datasets: sctx.datasets.clone(),
                                        },
                                    );
                                }
                                _ => {}
                            }
                        } else {
                            reg.register(tools::gold_model::GoldModelTool);
                            reg.register(
                                tools::apply_next_schema_batch::ApplyNextModelSchemaBatchTool {
                                    datasets: sctx.datasets.clone(),
                                },
                            );
                        }
                    }
                    reg.register(SqlRunTool {
                        query: query.clone(),
                    });
                    reg.register(tools::dbt_examples::SearchDbtExamplesTool);
                    reg.register(FilesTool {
                        datasets: sctx.datasets.clone(),
                    });
                    reg.register(JsonFileTool);

                    if matches!(
                        authoring_policy,
                        crate::data_engineer::authoring_driver::AuthoringToolPolicy::BatchingCleanseSql
                    ) {
                        let lines = vec![
                            "- apply_next_cleanse_batch(args:{instructions?:string})".to_string(),
                            "- file(args:{op:\"list\"|\"get\", prefix?:string, path?:string, limit?:int, max_chars?:int} | {op:\"patch\", path:string, patch_text:string} | {op:\"rm\", path:string, expected_sha256?:string} | {op:\"mv\", from:string, to:string, expected_sha256?:string})".to_string(),
                            "- json_file(args:{op:\"get_item\", path:string, pointer?:string} | {op:\"query\", path:string, pointer?:string, unique_id?:string, name?:string, resource_type?:string, limit?:int})".to_string(),
                            "- sql_schema / sql_stats / sql_sample / vect_query (discovery context)"
                                .to_string(),
                            "- run_sql (targeted probes)".to_string(),
                            "- artifacts".to_string(),
                        ];
                        tools_card = Self::build_tools_card(
                            "Allowed tools (authoring phase; plan-batched, deterministic):",
                            lines,
                            Vec::new(),
                            Some("Not available in this phase: staging_model (batch tool calls it deterministically), dbt_validate, publish_dbt_to_provider.".to_string()),
                        );
                    } else if matches!(
                        authoring_policy,
                        crate::data_engineer::authoring_driver::AuthoringToolPolicy::BatchingCleanseSchema
                    ) {
                        let lines = vec![
                            "- apply_next_cleanse_schema_batch(args:{instructions?:string})"
                                .to_string(),
                            "- file(args:{op:\"list\"|\"get\", prefix?:string, path?:string, limit?:int, max_chars?:int} | {op:\"patch\", path:string, patch_text:string} | {op:\"rm\", path:string, expected_sha256?:string} | {op:\"mv\", from:string, to:string, expected_sha256?:string})".to_string(),
                            "- json_file(args:{op:\"get_item\", path:string, pointer?:string} | {op:\"query\", path:string, pointer?:string, unique_id?:string, name?:string, resource_type?:string, limit?:int})".to_string(),
                            "- sql_schema / sql_stats / sql_sample / vect_query (discovery context)"
                                .to_string(),
                            "- run_sql (targeted probes)".to_string(),
                            "- artifacts".to_string(),
                        ];
                        tools_card = Self::build_tools_card(
                            "Allowed tools (authoring phase; plan-batched, deterministic):",
                            lines,
                            Vec::new(),
                            Some("Not available in this phase: staging_model, apply_next_cleanse_batch, dbt_validate, publish_dbt_to_provider.".to_string()),
                        );
                    } else if matches!(
                        authoring_policy,
                        crate::data_engineer::authoring_driver::AuthoringToolPolicy::BatchingModelSql
                    ) {
                        let lines = vec![
                            "- apply_next_model_batch(args:{instructions?:string})".to_string(),
                            "- file(args:{op:\"list\"|\"get\", prefix?:string, path?:string, limit?:int, max_chars?:int} | {op:\"patch\", path:string, patch_text:string} | {op:\"rm\", path:string, expected_sha256?:string} | {op:\"mv\", from:string, to:string, expected_sha256?:string})".to_string(),
                            "- json_file(args:{op:\"get_item\", path:string, pointer?:string} | {op:\"query\", path:string, pointer?:string, unique_id?:string, name?:string, resource_type?:string, limit?:int})".to_string(),
                            "- sql_schema / sql_stats / sql_sample / vect_query (discovery context)"
                                .to_string(),
                            "- run_sql (targeted probes)".to_string(),
                            "- artifacts".to_string(),
                        ];
                        tools_card = Self::build_tools_card(
                            "Allowed tools (authoring phase; plan-batched, deterministic):",
                            lines,
                            Vec::new(),
                            Some("Not available in this phase: gold_model (batch tool calls it deterministically), dbt_validate, publish_dbt_to_provider.".to_string()),
                        );
                    } else if matches!(
                        authoring_policy,
                        crate::data_engineer::authoring_driver::AuthoringToolPolicy::BatchingModelSchema
                    ) {
                        let lines = vec![
                            "- apply_next_model_schema_batch(args:{instructions?:string})"
                                .to_string(),
                            "- file(args:{op:\"list\"|\"get\", prefix?:string, path?:string, limit?:int, max_chars?:int} | {op:\"patch\", path:string, patch_text:string} | {op:\"rm\", path:string, expected_sha256?:string} | {op:\"mv\", from:string, to:string, expected_sha256?:string})".to_string(),
                            "- json_file(args:{op:\"get_item\", path:string, pointer?:string} | {op:\"query\", path:string, pointer?:string, unique_id?:string, name?:string, resource_type?:string, limit?:int})".to_string(),
                            "- sql_schema / sql_stats / sql_sample / vect_query (discovery context)"
                                .to_string(),
                            "- run_sql (targeted probes)".to_string(),
                            "- artifacts".to_string(),
                        ];
                        tools_card = Self::build_tools_card(
                            "Allowed tools (authoring phase; plan-batched, deterministic):",
                            lines,
                            Vec::new(),
                            Some("Not available in this phase: gold_model, apply_next_model_batch, dbt_validate, publish_dbt_to_provider.".to_string()),
                        );
                    } else {
                        let mut lines = vec![
                            "- sql_schema(args:{table?:string})".to_string(),
                            "- vect_query(args:{scope:\"dataset\"|\"field\"|\"doc\"|\"artifact\"|\"metric\"|\"model\", query_text:string, k:int})".to_string(),
                            "  - IMPORTANT: arg key is query_text (NOT query). scope must be one of the listed strings (NOT \"table\").".to_string(),
                            "- sql_stats(args:{table:string, field:string}) (requires field; no table-only mode)".to_string(),
                            "- sql_sample(args:{table:string, field:string, k:int}) (top values for a FIELD; not a row sampler)".to_string(),
                            "- run_sql(args:{sql:string}) (use this to sample rows: SELECT * FROM <table> LIMIT 20)".to_string(),
                        ];
                        if phase == control_flow::Phase::CleanseAuthor {
                            lines.push("- staging_model(args:{dataset_ids:[string], instructions?:string, sql?:string|staging_model?:string|expression?:string})".to_string());
                            lines.push("  - IMPORTANT: you MUST provide dataset_ids. This tool will NOT default to all datasets.".to_string());
                            lines.push(
                                "- apply_next_cleanse_schema_batch(args:{instructions?:string})"
                                    .to_string(),
                            );
                        } else {
                            lines.push("- gold_model(args:{items:[{name:string, folder?:\"marts\"|\"core\", goal?:string, description?:string, inputs:[string], instructions?:string}]})".to_string());
                            lines.push("  - IMPORTANT: max 5 items per call. Gold MUST use ref('stg_*') only; NO source().".to_string());
                            lines.push(
                                "- apply_next_model_schema_batch(args:{instructions?:string})"
                                    .to_string(),
                            );
                        }
                        lines.extend_from_slice(&[
                            "- file(args:{op:\"list\"|\"get\", prefix?:string, path?:string, limit?:int, max_chars?:int} | {op:\"patch\", path:string, patch_text:string} | {op:\"rm\", path:string, expected_sha256?:string} | {op:\"mv\", from:string, to:string, expected_sha256?:string})".to_string(),
                            "- json_file(args:{op:\"get_item\", path:string, pointer?:string} | {op:\"query\", path:string, pointer?:string, unique_id?:string, name?:string, resource_type?:string, limit?:int})".to_string(),
                        ]);
                        tools_card = Self::build_tools_card(
                            "Allowed tools (authoring phase):",
                            lines,
                            Vec::new(),
                            Some("Not available in this phase: dbt_validate, publish_dbt_to_provider (suite handles these deterministically).".to_string()),
                        );
                    }
                }
            }
            control_flow::Phase::CleanseReview
            | control_flow::Phase::ModelReview
            | control_flow::Phase::PostPublishReview => {
                // Review phases: keep read-only; do not allow arbitrary SQL execution.
                struct ReadOnlyFilesTool {
                    inner: FilesTool,
                }
                #[async_trait::async_trait]
                impl react_core::tools::Tool for ReadOnlyFilesTool {
                    fn name(&self) -> &'static str {
                        "file"
                    }
                    async fn call(
                        &self,
                        args: serde_json::Value,
                        ctx: &react_core::agent::AgentCtx,
                    ) -> Result<serde_json::Value, String> {
                        if !crate::data_engineer::tool_ops::is_file_read_op(&args) {
                            return Err("file is read-only in review phases; use op='get' or op='list' (mutating ops are disabled: patch/rm/mv)".to_string());
                        }
                        self.inner.call(args, ctx).await
                    }
                }
                reg.register(ReadOnlyFilesTool {
                    inner: FilesTool {
                        datasets: sctx.datasets.clone(),
                    },
                });
                reg.register(JsonFileTool);

                tools_card = Self::build_tools_card(
                    "Allowed tools (review phase, read-only):",
                    vec![
                        "- file (list/get)".to_string(),
                        "- json_file (get_item/query)".to_string(),
                        "- artifacts".to_string(),
                        "- sql_schema / sql_stats / sql_sample / vect_query (read-only context)"
                            .to_string(),
                    ],
                    Vec::new(),
                    Some("Not available: run_sql, staging_model, approve_and_save_artifact(_batch), dbt_validate, publish_dbt_to_provider.".to_string()),
                );
            }
            _ => {
                // Other phases do not run an LLM action set (suite does deterministic steps).
                tools_card =
                    "Allowed tools: (suite deterministic step; no agent tools)".to_string();
            }
        }

        Ok((reg, tools_card))
    }

    /// Hard cutover: refresh canonical catalog state on every run.
    ///
    /// This builds/refreshes warehouse-backed catalog artifacts and then enforces that required
    /// metadata exists so planning can treat catalog as canonical.
    async fn ensure_catalog_bootstrap(sctx: &SuiteCtx) -> Result<CatalogBootstrapOutcome, String> {
        let (Some(cat), Some(datasets)) = (sctx.catalog.as_ref(), sctx.datasets.as_ref()) else {
            return Ok(CatalogBootstrapOutcome {
                metadata_complete: true,
            });
        };
        let dss = datasets
            .list_datasets()
            .await
            .map_err(|e| {
                tracing::error!(
                    error = %e,
                    "data_engineer: catalog bootstrap dataset discovery failed"
                );
                format!("catalog bootstrap failed: dataset discovery error: {e}")
            })?;
        if dss.is_empty() {
            return Err("catalog bootstrap failed: dataset discovery returned zero datasets (check AWS credentials/region and warehouse schema config)".to_string());
        }

        // Also detect whether the global semantic context exists.
        let global_key = sctx.keyspace.semantic_key(
            &sctx.scope,
            react_core::providers::catalog::types::GLOBAL_SEMANTIC_DATASET_ID,
        );
        tracing::info!(
            "data_engineer: refreshing canonical catalogs/stats for {} dataset(s)",
            dss.len()
        );
        let empty: HashMap<String, react_core::discover::Metadata> = HashMap::new();
        cat.build_all_with_progress(&sctx.scope, datasets.as_ref(), &empty, None)
            .await
            .map_err(|e| format!("catalog bootstrap failed while building catalogs: {e}"))?;

        // Mandatory metadata completion pass.
        let mut all: HashMap<String, react_core::discover::Metadata> = HashMap::new();
        for ds in dss.iter() {
            all.insert(ds.fqn(), react_core::discover::Metadata::default());
        }
        let enrich_report = cat
            .run_llm_enrichment_all(&sctx.scope, &all)
            .await
            .map_err(|e| format!("catalog bootstrap failed while enriching metadata: {e}"))?;
        tracing::info!(
            "data_engineer: catalog enrichment summary datasets={} ok={} failed={} global_written={}",
            enrich_report.dataset_total,
            enrich_report.dataset_enriched_ok,
            enrich_report.dataset_enriched_failed,
            enrich_report.global_context_written
        );

        // Single metadata gate + single deterministic repair attempt.
        // If metadata still doesn't fully converge, continue to planning with warnings.
        let collect_meta_errors = || async {
            let mut errs: Vec<String> = Vec::new();
            for ds in dss.iter() {
                let id = ds.fqn();
                let Some(c) = cat.read_catalog(&sctx.scope, &id).await.map_err(|e| {
                    format!("catalog bootstrap failed while reading catalog for {id}: {e}")
                })?
                else {
                    errs.push(format!("{id}: catalog missing after refresh"));
                    continue;
                };
                if c.description
                    .as_deref()
                    .map(|s| s.trim().is_empty())
                    .unwrap_or(true)
                {
                    errs.push(format!("{id}: missing dataset description"));
                }
                let missing_fields = c
                    .fields
                    .iter()
                    .filter(|f| {
                        f.description
                            .as_deref()
                            .map(|s| s.trim().is_empty())
                            .unwrap_or(true)
                    })
                    .count();
                if missing_fields > 0 {
                    errs.push(format!(
                        "{id}: {} field(s) missing field descriptions",
                        missing_fields
                    ));
                }
            }
            let gctx = sctx.storage.get_json(&global_key).await.ok().and_then(|v| {
                serde_json::from_value::<
                    react_core::providers::catalog::types::GlobalSemanticContext,
                >(v)
                .ok()
            });
            match gctx {
                Some(g) => {
                    if g.audiences.is_empty() {
                        errs.push("global_semantic_context: audiences is empty".to_string());
                    }
                    if g.context_bullets.is_empty() {
                        errs.push("global_semantic_context: context_bullets is empty".to_string());
                    }
                }
                None => errs.push("global_semantic_context: missing".to_string()),
            }
            Ok::<Vec<String>, String>(errs)
        };

        let mut meta_errors = collect_meta_errors().await?;
        if !meta_errors.is_empty() {
            tracing::warn!(
                "data_engineer: catalog metadata gate failed; applying single deterministic repair attempt:\n- {}",
                meta_errors.join("\n- ")
            );
            // Deterministic catalog description repair.
            for ds in dss.iter() {
                let id = ds.fqn();
                let Some(mut c) = cat.read_catalog(&sctx.scope, &id).await.map_err(|e| {
                    format!("catalog bootstrap failed while reading catalog for {id}: {e}")
                })?
                else {
                    continue;
                };
                let mut changed = false;
                if c.description
                    .as_deref()
                    .map(|s| s.trim().is_empty())
                    .unwrap_or(true)
                {
                    let field_preview = c
                        .fields
                        .iter()
                        .map(|f| f.name.clone())
                        .take(5)
                        .collect::<Vec<_>>()
                        .join(", ");
                    c.description = Some(if field_preview.is_empty() {
                        format!(
                            "Dataset {} contains source records used for analytics modeling.",
                            id
                        )
                    } else {
                        format!(
                            "Dataset {} contains source records with fields {} for analytics modeling.",
                            id, field_preview
                        )
                    });
                    changed = true;
                }
                for f in c.fields.iter_mut() {
                    if f.description
                        .as_deref()
                        .map(|s| s.trim().is_empty())
                        .unwrap_or(true)
                    {
                        f.description =
                            Some(format!("Field {} in dataset {}.", f.name, c.dataset_id));
                        changed = true;
                    }
                }
                if changed {
                    cat.write_catalog(&sctx.scope, &id, &c)
                        .await
                        .map_err(|e| format!("catalog bootstrap failed while writing {id}: {e}"))?;
                }
            }
            // Deterministic global semantic context repair.
            let gctx = sctx.storage.get_json(&global_key).await.ok().and_then(|v| {
                serde_json::from_value::<
                    react_core::providers::catalog::types::GlobalSemanticContext,
                >(v)
                .ok()
            });
            let needs_global_defaults = match gctx {
                Some(ref g) => g.audiences.is_empty() || g.context_bullets.is_empty(),
                None => true,
            };
            if needs_global_defaults {
                let dataset_ids = dss.iter().map(|d| d.fqn()).collect::<Vec<_>>();
                let default_global = react_core::providers::catalog::types::GlobalSemanticContext {
                    version: 1,
                    built_at_epoch_secs: Some((chrono::Utc::now().timestamp()).max(0) as u64),
                    audiences: vec![react_core::providers::catalog::types::GlobalAudience {
                        audience: "Analytics engineering and data consumers".to_string(),
                        confidence: 0.90,
                        evidence: dataset_ids
                            .iter()
                            .take(5)
                            .map(|d| format!("dataset_id={}", d))
                            .collect(),
                    }],
                    context_bullets: vec![
                        react_core::providers::catalog::types::GlobalContextBullet {
                            text: "Project models warehouse datasets for analytics use-cases."
                                .to_string(),
                            confidence: 0.90,
                            evidence: dataset_ids
                                .iter()
                                .take(5)
                                .map(|d| format!("dataset_id={}", d))
                                .collect(),
                        },
                    ],
                    dataset_groups: vec![],
                    assumptions_and_gaps: vec![],
                };
                let value = serde_json::to_value(default_global)
                    .map_err(|e| format!("catalog bootstrap failed while encoding global semantic context defaults: {e}"))?;
                sctx.storage
                    .put_json(&global_key, &value)
                    .await
                    .map_err(|e| format!("catalog bootstrap failed while writing global semantic context defaults: {e}"))?;
            }
            meta_errors = collect_meta_errors().await?;
            if !meta_errors.is_empty() {
                tracing::warn!(
                    "data_engineer: catalog metadata still incomplete after single repair attempt; proceeding to planning with defaults best-effort:\n- {}",
                    meta_errors.join("\n- ")
                );
            }
        }
        Ok(CatalogBootstrapOutcome {
            metadata_complete: meta_errors.is_empty(),
        })
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
        let tools_card = Self::build_tools_card_for_agent_type(AgentMode::Ask);

        let pf = crate::preflight::CatalogPreflightProvider {
            discovery_limits: crate::preflight::discovery::DiscoveryLimits::default(),
            run_preflight_on_bundle: false,
        };
        let bundle = pf.run(thread_id, question, "ask", sctx).await.discovery;

        let registry = Self::build_tools(AgentMode::Ask, sctx)?;
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
            resolved_config: sctx.resolved_config.clone(),
        };

        match Agent::run_until_block(
            &registry,
            &actx,
            &sys,
            &tools_card,
            question,
            LlmCallOptions {
                prompt_id: "data_engineer.ask_user_parse",
                thread_id: None,
                expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
                max_output_tokens: None,
                temperature: None,
                top_p: None,
                reasoning_effort: None,
            },
        )
        .await
        {
            Ok(RunOutcome::Final {
                thread_id: _tid,
                result,
            }) => Ok(vec![FlowFrame::Final {
                kind: result.kind.into(),
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
        Self::ensure_catalog_bootstrap_semaphored(thread_id, sctx).await?;
        let sys = crate::util::time_context::with_time_context(prompts::review_system_prompt());
        let tools_card = Self::build_tools_card_for_agent_type(AgentMode::Review);

        let registry = Self::build_tools(AgentMode::Review, sctx)?;
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
            resolved_config: sctx.resolved_config.clone(),
        };

        let prompt = Self::inject_review_question(question);
        match Agent::run_until_block(
            &registry,
            &actx,
            &sys,
            &tools_card,
            &prompt,
            LlmCallOptions {
                prompt_id: "data_engineer.ask_approval_parse",
                thread_id: None,
                expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
                max_output_tokens: None,
                temperature: None,
                top_p: None,
                reasoning_effort: None,
            },
        )
        .await
        {
            Ok(RunOutcome::Final {
                thread_id: _tid,
                result,
            }) => Ok(vec![FlowFrame::Final {
                kind: result.kind.into(),
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
            resolved_config: sctx.resolved_config.clone(),
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
            // Non-deterministic single-pass modes may interrupt via ask_user/ask_approval.
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
            resolved_config: sctx.resolved_config.clone(),
        }
    }

    async fn run_agent(
        thread_id: &str,
        question: &str,
        sctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String> {
        use control_flow::{DerivedGuardState, Phase};
        if let Err(e) = Self::ensure_catalog_bootstrap_semaphored(thread_id, sctx).await {
            let thread_store = ThreadStore::new(
                sctx.storage.clone(),
                sctx.scope.clone(),
                sctx.keyspace.clone(),
            );
            let reason = format!(
                "catalog bootstrap metadata gate failed before planning:\n{}",
                e.trim()
            );
            tracing::error!(
                thread_id = %thread_id,
                error = %e,
                "data_engineer: refusing to continue after preflight catalog metadata gate failure"
            );
            let _ = apply_guard_block(
                &thread_store,
                thread_id,
                Phase::Preflight,
                GuardBlockKind::PrecheckFailed,
                reason.clone(),
            )
            .await;
            return Err(format!("agent_mode_await_user_forbidden: {}", reason));
        }

        // Phase-step budget is reset when we make clear forward progress (phase advances).
        // This prevents aborting a healthy thread that is steadily moving through phases,
        // while still bounding degenerate loops.
        let max_phase_steps: usize = std::env::var("AGENT_MAX_PHASE_STEPS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(40)
            .max(8)
            .min(400);
        let max_replan_backtracks: usize = std::env::var("AGENT_MAX_REPLAN_BACKTRACKS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(3)
            .max(2)
            .min(20);
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

            let mut execution_state = crate::data_engineer::progress_controller::ExecutionState::load_strict(
                &thread_store,
                thread_id,
            )
            .await?
            .unwrap_or_else(crate::data_engineer::progress_controller::ExecutionState::new);
            if execution_state.mode
                == crate::data_engineer::progress_controller::ExecutionMode::Failed
                && execution_state.current_phase != Some(control_flow::Phase::Done)
            {
                execution_state.mode = if matches!(
                    execution_state.current_phase,
                    Some(control_flow::Phase::CleanseAuthor | control_flow::Phase::ModelAuthor)
                ) {
                    crate::data_engineer::progress_controller::ExecutionMode::Mutate
                } else {
                    crate::data_engineer::progress_controller::ExecutionMode::Discover
                };
                let _ = execution_state.save(&thread_store, thread_id).await;
            }
            let phase = execution_state
                .current_phase
                .unwrap_or(control_flow::Phase::Preflight);
            let thread_state_step_count = thread_store
                .get_thread_state(thread_id)
                .await
                .ok()
                .map(|st| st.last_materialized_step_count)
                .unwrap_or(0);
            let idx = phase_index(phase);
            if idx > max_phase_idx_seen {
                max_phase_idx_seen = idx;
                // Reset the budget when we advance phases (i.e. not looping).
                remaining_steps = max_phase_steps;
            }
            let guard: DerivedGuardState =
                control_flow::derive_guard_state_from_execution_state(&execution_state);
            // Agent mode is non-interactive: never expose ask-approval/ask-user pathways.
            let allow_ask_approval = false;
            match crate::data_engineer::phase_gate::evaluate_pre_turn_directive(
                &execution_state,
                phase,
                max_replan_backtracks,
            ) {
                crate::data_engineer::phase_gate::PreTurnDirective::Proceed => {}
                crate::data_engineer::phase_gate::PreTurnDirective::FailFast { kind, reason } => {
                    crate::data_engineer::transition_dispatcher::apply_phase_directive(
                        &thread_store,
                        thread_id,
                        Some("agent".to_string()),
                        Some(phase),
                        crate::data_engineer::transition_dispatcher::PhaseDirective::Block {
                            phase,
                            kind,
                            reason: reason.clone(),
                        },
                    )
                    .await?;
                    let mut es = execution_state.clone();
                    es.mark_failed(reason.clone());
                    let _ = es.save(&thread_store, thread_id).await;
                    return Err(reason);
                }
            }

            // Hard-cutover: validate failure context is sourced from typed execution state only.
            let mut last_validate_brief: Option<String> = None;
            let mut last_validate_failed_models: Vec<
                crate::data_engineer::progress_controller::FailedModelRef,
            > = Vec::new();
            if let Some(last) = execution_state.last_validate.as_ref() {
                if let Some(brief) = last.brief.as_ref().filter(|s| !s.trim().is_empty()) {
                    last_validate_brief = Some(brief.clone());
                }
                if !last.failed_models.is_empty() {
                    last_validate_failed_models = last.failed_models.clone();
                }
            }

            match phase {
                Phase::Preflight => match Self::execute_preflight_phase(&thread_store, thread_id, sctx).await? {
                    PhaseExecutorOutcome::Continue => continue,
                    PhaseExecutorOutcome::Return(frames) => return Ok(frames),
                },

                Phase::CleansePlan | Phase::ModelPlan => {
                    match Self::execute_plan_phase(
                        &thread_store,
                        thread_id,
                        phase,
                        question,
                        sctx,
                        &execution_state,
                        &guard,
                        allow_ask_approval,
                        thread_state_step_count,
                        &last_validate_brief,
                        &last_validate_failed_models,
                    )
                    .await? {
                        PhaseExecutorOutcome::Continue => continue,
                        PhaseExecutorOutcome::Return(frames) => return Ok(frames),
                    }
                }

                Phase::CleanseAuthor | Phase::ModelAuthor => {
                    match Self::execute_author_phase(
                        &thread_store,
                        thread_id,
                        phase,
                        question,
                        sctx,
                        &execution_state,
                        &guard,
                        allow_ask_approval,
                        thread_state_step_count,
                        &last_validate_brief,
                        &last_validate_failed_models,
                    )
                    .await? {
                        PhaseExecutorOutcome::Continue => continue,
                        PhaseExecutorOutcome::Return(frames) => return Ok(frames),
                    }
                }

                Phase::CleanseValidate | Phase::ModelValidate => {
                    match Self::execute_validate_phase(
                        &thread_store,
                        thread_id,
                        phase,
                        question,
                        sctx,
                        &execution_state,
                        &guard,
                        allow_ask_approval,
                        thread_state_step_count,
                        &last_validate_brief,
                        &last_validate_failed_models,
                    )
                    .await? {
                        PhaseExecutorOutcome::Continue => continue,
                        PhaseExecutorOutcome::Return(frames) => return Ok(frames),
                    }
                }

                Phase::CleanseReview | Phase::ModelReview | Phase::PostPublishReview => {
                    match Self::execute_review_phase(
                        &thread_store,
                        thread_id,
                        phase,
                        question,
                        sctx,
                        &execution_state,
                        &mut out_frames,
                    )
                    .await? {
                        PhaseExecutorOutcome::Continue => continue,
                        PhaseExecutorOutcome::Return(frames) => return Ok(frames),
                    }
                }

                Phase::PublishAwaitApproval => match Self::execute_publish_await_approval_phase(
                    &thread_store,
                    thread_id,
                    sctx,
                )
                .await? {
                    PhaseExecutorOutcome::Continue => continue,
                    PhaseExecutorOutcome::Return(frames) => return Ok(frames),
                },

                Phase::Publish => match Self::execute_publish_phase(&thread_store, thread_id, sctx).await? {
                    PhaseExecutorOutcome::Continue => continue,
                    PhaseExecutorOutcome::Return(frames) => return Ok(frames),
                },

                Phase::Done => match Self::execute_done_phase(&mut out_frames)? {
                    PhaseExecutorOutcome::Continue => continue,
                    PhaseExecutorOutcome::Return(frames) => return Ok(frames),
                },
            }
        }

        let mut budget_msg = format!(
            "headless_budget_exhausted: Agent reached the phase-step budget without completing.\n\nBudget:\n- max_steps_per_progress={max_phase_steps}\n- total_steps={total_steps}\n\nThis indicates a loop (re-entering phases without durable progress)."
        );
        if let Some(mut es) =
            crate::data_engineer::progress_controller::ExecutionState::load(&thread_store, thread_id)
                .await
        {
            budget_msg.push_str(&format!(
                "\n\nExecution state at exhaustion:\n- current_phase={}\n- mode={}\n- phase_reason_code={}\n- replan_backtracks={}\n- stall_count={}/{}\n- hard_mutation_repair_mode={}",
                es.current_phase.map(|p| p.as_str().to_string()).unwrap_or_else(|| "null".to_string()),
                format!("{:?}", es.mode),
                es.phase_reason_code.map(|c| c.as_str().to_string()).unwrap_or_else(|| "null".to_string()),
                es.replan_backtracks,
                es.stall_count,
                es.max_stall_count,
                es.hard_mutation_repair_mode,
            ));
            es.mark_failed(budget_msg.clone());
            let _ = es.save(&thread_store, thread_id).await;
        }
        Err(budget_msg)
    }

    async fn run_authoring(
        kind: AuthoringKind,
        thread_id: &str,
        question: &str,
        sctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String> {
        Self::ensure_catalog_bootstrap_semaphored(thread_id, sctx).await?;

        let (agent_name, sys, tools_card, run_preflight_on_bundle) = match kind {
            AuthoringKind::Cleanse => (
                "cleanse",
                crate::util::time_context::with_time_context(prompts::cleanse_system_prompt()),
                Self::build_tools_card_for_agent_type(AgentMode::Cleanse),
                false,
            ),
            AuthoringKind::Model => (
                "model",
                crate::util::time_context::with_time_context(prompts::model_system_prompt()),
                Self::build_tools_card_for_agent_type(AgentMode::Model),
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

        let agent_mode = AgentMode::parse(agent_name)?;
        let registry = Self::build_tools(agent_mode, sctx)?;
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
            resolved_config: sctx.resolved_config.clone(),
        };

        let mut last_final: Option<react_core::session::ThreadResult> = None;
        let mut prompt = match kind {
            AuthoringKind::Cleanse => Self::inject_cleanse_question(question),
            AuthoringKind::Model => Self::inject_model_question(question),
        };

        let llm_options = match kind {
            AuthoringKind::Cleanse => LlmCallOptions {
                prompt_id: "data_engineer.cleanse_author",
                thread_id: None,
                expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
                temperature: Some(0.05),
                top_p: Some(1.0),
                max_output_tokens: Some(
                    std::env::var("LLM_AUTHOR_MAX_TOKENS_CLEANSE")
                        .ok()
                        .and_then(|s| s.parse::<u32>().ok())
                        .unwrap_or(12_000)
                        .max(2_000)
                        .min(64_000),
                ),
                reasoning_effort: None,
            },
            AuthoringKind::Model => LlmCallOptions {
                prompt_id: "data_engineer.model_author",
                thread_id: None,
                expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
                temperature: Some(0.12),
                top_p: Some(1.0),
                max_output_tokens: Some(
                    std::env::var("LLM_AUTHOR_MAX_TOKENS_MODEL")
                        .ok()
                        .and_then(|s| s.parse::<u32>().ok())
                        .unwrap_or(16_000)
                        .max(2_000)
                        .min(64_000),
                ),
                reasoning_effort: None,
            },
        };

        for attempt in 0..10 {
            match Agent::run_until_block(
                &registry,
                &actx,
                &sys,
                &tools_card,
                &prompt,
                llm_options.clone(),
            )
            .await
            {
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
                            kind: result.kind.into(),
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
                        return Err(format!(
                            "dbt_validate failed due to a warehouse/aws configuration issue: {}",
                            brief
                        ));
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
                             IMPORTANT: Your very next step MUST be a FIX to dbt artifacts (prefer fixing silver models under models/staging/; do NOT relax/remove tests unless nullable-by-design is justified).\n\
                             Recommended flow:\n\
                             - Use `json_file op=query` on `target/manifest.json` (pointer=/nodes) to locate failing test/model nodes (by name/resource_type).\n\
                               - Find the failing test node(s), then follow depends_on to the referenced model node.\n\
                               - From the model node, compute the physical relation: <database>.<schema>.<alias>.\n\
                             - Use `sql_schema` on that relation to determine the tested column type.\n\
                             - Use `run_sql` to probe the actual data before editing:\n\
                               - Null check: SELECT count(*) AS total, count_if({{col}} IS NULL) AS nulls FROM {{relation}}\n\
                               - If string-ish: SELECT count_if(trim(cast({{col}} AS varchar)) = '') AS empty FROM {{relation}}\n\
                               - If time-like by type: SELECT count_if(try_cast(nullif(trim(cast({{col}} AS varchar)), '') AS timestamp) IS NULL) AS unparseable FROM {{relation}}\n\
                               - Sample failing: SELECT {{col}} FROM {{relation}} WHERE {{col}} IS NULL LIMIT 50\n\
                             - Apply a fix using `staging_model` or `file op=patch|rm|mv`.\n\
                             - You MUST NOT claim fixed unless a probe query shows the failure condition is now 0 rows.\n\
                             Only AFTER applying a fix should you re-run `dbt_validate` with build=true.",
                            attempt + 1,
                            lines.join("\n"),
                            manifest_block
                        );
                    } else {
                        prompt = format!(
                            "Auto-remediation attempt {}: dbt_validate/build failed.\n\nError summary:\n{}\n\nAutomatically fix the DBT project:\n- Prefer calling `staging_model` to update silver models under models/staging/ (nested fields, cleansing, naming).\n- Use file or artifacts to inspect/edit existing files.\n- Re-run dbt_validate with build=true.\nRepeat until compile_ok=true AND run_ok=true.",
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
                    return Err(format!("await_user_forbidden: {}", p));
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
                kind: r.kind.into(),
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

    fn label(&self) -> &'static str {
        "Data Engineer"
    }

    fn supported_agent_types(&self) -> Vec<String> {
        vec![
            "ask".to_string(),
            "agent".to_string(),
            "review".to_string(),
        ]
    }

    fn default_agent_type(&self) -> &'static str {
        "ask"
    }

    fn phase_order(&self, agent_type: &str) -> Vec<String> {
        // Only expose phases for agent-mode; other modes are single-pass.
        if AgentMode::parse(agent_type).ok() != Some(AgentMode::Agent) {
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
        let agent_mode = Self::validate_agent_type(agent_type)?;
        let frames = match agent_mode {
            AgentMode::Agent => Self::run_agent(thread_id, question, ctx).await,
            AgentMode::Model => Self::run_model(thread_id, question, ctx).await,
            AgentMode::Cleanse => Self::run_cleanse(thread_id, question, ctx).await,
            AgentMode::Review => Self::run_review(thread_id, question, ctx).await,
            AgentMode::Ask => Self::run_ask(thread_id, question, ctx).await,
        }?;
        Self::enforce_non_interactive_contract(agent_mode, frames)
    }

    async fn handle_open(
        &self,
        thread_id: &str,
        question: &str,
        agent_type: &str,
        ctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String> {
        let agent_mode = Self::validate_agent_type(agent_type)?;
        let frames = match agent_mode {
            AgentMode::Agent => Self::run_agent(thread_id, question, ctx).await,
            AgentMode::Model => Self::run_model(thread_id, question, ctx).await,
            AgentMode::Cleanse => Self::run_cleanse(thread_id, question, ctx).await,
            AgentMode::Review => Self::run_review(thread_id, question, ctx).await,
            AgentMode::Ask => Self::run_ask(thread_id, question, ctx).await,
        }?;
        Self::enforce_non_interactive_contract(agent_mode, frames)
    }

    async fn handle_user(
        &self,
        thread_id: &str,
        text: &str,
        agent_type: &str,
        ctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String> {
        let agent_mode = Self::validate_agent_type(agent_type)?;
        let frames = match agent_mode {
            AgentMode::Agent => Self::run_agent(thread_id, text, ctx).await,
            AgentMode::Model => Self::run_model(thread_id, text, ctx).await,
            AgentMode::Cleanse => Self::run_cleanse(thread_id, text, ctx).await,
            AgentMode::Review => Self::run_review(thread_id, text, ctx).await,
            AgentMode::Ask => Self::run_ask(thread_id, text, ctx).await,
        }?;
        Self::enforce_non_interactive_contract(agent_mode, frames)
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

    #[test]
    fn select_high_value_model_candidates_uses_score_threshold_not_fixed_count() {
        let candidates: Vec<crate::data_engineer::plan_schema::ModelPlanCandidateV1> = (0..10)
            .map(|i| crate::data_engineer::plan_schema::ModelPlanCandidateV1 {
                name: format!("m_{i}"),
                insight: "x".to_string(),
                observation: "y".to_string(),
                value_score: 95 - (i as i32 * 5),
            })
            .collect();
        let selected = DataEngineerSuite::select_high_value_model_candidates(&candidates);
        assert_eq!(selected.len(), 6, "default min score 70 should keep 6");
        assert_eq!(selected[0].name, "m_0");
        assert_eq!(selected[5].name, "m_5");
    }

    #[test]
    fn compile_model_candidates_plan_builds_batches_from_selected_threshold() {
        let cands = crate::data_engineer::plan_schema::ModelPlanCandidatesV1 {
            candidates: (0..13)
                .map(|i| crate::data_engineer::plan_schema::ModelPlanCandidateV1 {
                    name: format!("m_{i}"),
                    insight: "high value".to_string(),
                    observation: "grounded".to_string(),
                    value_score: 100 - (i as i32 * 5),
                })
                .collect(),
        };
        let plan = DataEngineerSuite::compile_model_candidates_plan(&cands);
        let tasks = plan.tasks;
        let batches = plan.batches;
        assert_eq!(tasks.len(), 7, "default min score 70 should keep 7");
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), 5);
        assert_eq!(batches[1].len(), 2);
    }

    #[test]
    fn parse_impl_spec_with_sanitize_strips_unknown_cleanse_keys() {
        let raw = serde_json::json!({
            "spec_version": 1,
            "row_preserving": true,
            "output_fields": [],
            "prohibited_ops": [],
            "batch_id": "x",
            "data_quality": {"checks":[]}
        });
        let (spec, stripped) = DataEngineerSuite::parse_impl_spec_value_with_sanitize::<
            crate::data_engineer::plan::CleanseImplementationSpec,
        >(raw, true)
        .expect("cleanse spec should parse after sanitize");
        assert_eq!(spec.spec_version, 1);
        assert!(stripped.iter().any(|k| k == "batch_id"));
        assert!(stripped.iter().any(|k| k == "data_quality"));
    }

    #[test]
    fn parse_impl_spec_with_sanitize_strips_unknown_model_keys() {
        let raw = serde_json::json!({
            "spec_version": 1,
            "grain": "1 row per id",
            "inputs": [],
            "joins": [],
            "metrics": [],
            "output_fields": [],
            "assumptions": [],
            "dependencies": ["x"],
            "batch_id": "b1"
        });
        let (spec, stripped) = DataEngineerSuite::parse_impl_spec_value_with_sanitize::<
            crate::data_engineer::plan::ModelImplementationSpec,
        >(raw, false)
        .expect("model spec should parse after sanitize");
        assert_eq!(spec.spec_version, 1);
        assert!(stripped.iter().any(|k| k == "dependencies"));
        assert!(stripped.iter().any(|k| k == "batch_id"));
    }

    #[tokio::test]
    async fn review_registry_is_read_only() {
        let mut sctx = SuiteCtx::default();
        sctx.query = Some(Arc::new(MockQuery));

        let reg = DataEngineerSuite::build_tools(AgentMode::Review, &sctx)
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
            None,
            false,
        )
        .expect("build_tools_for_phase should succeed");

        // Tool card should advertise direct overwrite patching (not apply_next_* tools).
        assert!(card.contains("patch_text"));
        assert!(!card.contains("apply_next_model_batch"));

        // run_sql should not be available in hard mutation-only mode
        assert!(reg
            .call("run_sql", serde_json::json!({"sql":"SELECT 1"}), &actx)
            .await
            .is_ok());

        // file get should be blocked (put-only wrapper)
        assert!(reg
            .call(
                "file",
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
    async fn hard_mutation_run_sql_records_probe_attempts_to_execution_state() {
        let mut sctx = SuiteCtx::default();
        sctx.query = Some(Arc::new(MockQuery));
        let actx = DataEngineerSuite::agent_tool_ctx("probe-thread", &sctx);
        let store = actx.thread_store.as_ref().expect("thread_store");

        let mut st = crate::data_engineer::progress_controller::ExecutionState::new();
        st.last_validate_ok = Some(false);
        st.hard_mutation_repair_mode = true;
        st.probe_state.required = true;
        st.save(store, "probe-thread").await.expect("save state");

        let guard = crate::data_engineer::control_flow::DerivedGuardState {
            last_validate_failed: true,
            mutated_since_fail: false,
            patched_since_fail: false,
            mutation_failures_since_validate: 0,
            probe_required: false,
            probe_satisfied: false,
        };
        let (reg, _) = DataEngineerSuite::build_tools_for_phase(
            crate::data_engineer::control_flow::Phase::ModelAuthor,
            &guard,
            true,
            &sctx,
            None,
            None,
            false,
        )
        .expect("build_tools_for_phase should succeed");

        let _ = reg
            .call(
                "run_sql",
                serde_json::json!({"sql":"SELECT count(*) FROM some_table"}),
                &actx,
            )
            .await
            .expect("run_sql should execute");

        let updated = crate::data_engineer::progress_controller::ExecutionState::load(store, "probe-thread")
            .await
            .expect("state should load");
        assert_eq!(updated.probe_state.attempts_total, 1);
        assert_eq!(updated.probe_state.meaningful_attempts, 1);
    }

    #[tokio::test]
    async fn hard_mutation_run_sql_is_blocked_after_probe_exhaustion() {
        let mut sctx = SuiteCtx::default();
        sctx.query = Some(Arc::new(MockQuery));
        let actx = DataEngineerSuite::agent_tool_ctx("probe-exhausted-thread", &sctx);
        let store = actx.thread_store.as_ref().expect("thread_store");

        let mut st = crate::data_engineer::progress_controller::ExecutionState::new();
        st.last_validate_ok = Some(false);
        st.hard_mutation_repair_mode = true;
        st.probe_state.required = true;
        let sig = crate::data_engineer::progress_controller::ProbeSignature::from_run_sql(
            "select * from t limit 10",
            &serde_json::json!({"ok":true}),
        );
        let _ = st.note_probe_attempt("select * from t limit 10", true, sig.clone());
        let _ = st.note_probe_attempt("select * from t limit 10", true, sig.clone());
        let _ = st.note_probe_attempt("select * from t limit 10", true, sig.clone());
        let _ = st.note_probe_attempt("select * from t limit 10", true, sig);
        st.save(store, "probe-exhausted-thread")
            .await
            .expect("save state");

        let guard = crate::data_engineer::control_flow::DerivedGuardState {
            last_validate_failed: true,
            mutated_since_fail: false,
            patched_since_fail: false,
            mutation_failures_since_validate: 0,
            probe_required: false,
            probe_satisfied: false,
        };
        let (reg, _) = DataEngineerSuite::build_tools_for_phase(
            crate::data_engineer::control_flow::Phase::ModelAuthor,
            &guard,
            true,
            &sctx,
            None,
            None,
            false,
        )
        .expect("build_tools_for_phase should succeed");

        let err = reg
            .call(
                "run_sql",
                serde_json::json!({"sql":"SELECT count(*) FROM some_table"}),
                &actx,
            )
            .await
            .expect_err("run_sql should be blocked after exhaustion");
        assert!(err.contains("probe loop exhausted"));
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
            Some(super::AllowedBatch::CleanseSqlDatasetIds(vec![
                "AwsDataCatalog.db.t1".to_string(),
            ])),
            None,
            false,
        )
        .expect("build_tools_for_phase should succeed");

        assert!(card.contains("patch_text"));
        assert!(!card.contains("apply_next_cleanse_batch"));

        let err = reg
            .call("apply_next_cleanse_batch", serde_json::json!({}), &actx)
            .await
            .unwrap_err();
        assert!(err.contains("unknown tool"));
    }

    #[tokio::test]
    async fn hard_mutation_single_target_hides_schema_batch_tools() {
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
            Some(super::AllowedBatch::CleanseSchemaDatasetIds(vec![
                "AwsDataCatalog.db.t1".to_string(),
            ])),
            Some("models/staging/stg_test_raw_raw_order_items.sql".to_string()),
            false,
        )
        .expect("build_tools_for_phase should succeed");

        assert!(
            !card.contains("apply_next_cleanse_schema_batch"),
            "single-target SQL repair mode must not expose schema batch tools"
        );
        let err = reg
            .call(
                "apply_next_cleanse_schema_batch",
                serde_json::json!({}),
                &actx,
            )
            .await
            .unwrap_err();
        assert!(err.contains("unknown tool"));
    }

    #[tokio::test]
    async fn hard_mutation_mode_single_target_repair_rejects_other_paths() {
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

        let (reg, _card) = DataEngineerSuite::build_tools_for_phase(
            crate::data_engineer::control_flow::Phase::ModelAuthor,
            &guard,
            true,
            &sctx,
            None,
            Some("models/marts/fct_orders.sql".to_string()),
            false,
        )
        .expect("build_tools_for_phase should succeed");

        let err = reg
            .call(
                "file",
                serde_json::json!({
                    "op":"patch",
                    "path":"models/marts/fct_customers.sql",
                    "patch_text":"@@\n- select 1 as id\n+ select 1 as id\n"
                }),
                &actx,
            )
            .await
            .unwrap_err();
        assert!(err.contains("single-target repair mode violation"));
    }

    #[tokio::test]
    async fn hard_mutation_mode_single_target_allows_rm_mv_on_target_only() {
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

        let (reg, _card) = DataEngineerSuite::build_tools_for_phase(
            crate::data_engineer::control_flow::Phase::ModelAuthor,
            &guard,
            true,
            &sctx,
            None,
            Some("models/marts/fct_orders.sql".to_string()),
            false,
        )
        .expect("build_tools_for_phase should succeed");

        let err_off_target = reg
            .call(
                "file",
                serde_json::json!({"op":"rm","path":"models/marts/fct_other.sql"}),
                &actx,
            )
            .await
            .unwrap_err();
        assert!(err_off_target.contains("single-target repair mode violation"));

        // Targeted rm/mv are allowed by policy (they may still fail on missing file in this test context).
        let res_target_rm = reg
            .call(
                "file",
                serde_json::json!({"op":"rm","path":"models/marts/fct_orders.sql"}),
                &actx,
            )
            .await;
        if let Err(e) = res_target_rm {
            assert!(!e.contains("single-target repair mode violation"));
        }

        let res_target_mv = reg
            .call(
                "file",
                serde_json::json!({"op":"mv","from":"models/marts/fct_orders.sql","to":"models/marts/fct_orders_renamed.sql"}),
                &actx,
            )
            .await;
        if let Err(e) = res_target_mv {
            assert!(!e.contains("single-target repair mode violation"));
        }
    }

    #[tokio::test]
    async fn agent_phase_tool_card_and_registry_never_expose_ask_user() {
        let mut sctx = SuiteCtx::default();
        sctx.query = Some(Arc::new(MockQuery));
        let actx = DataEngineerSuite::agent_tool_ctx("t", &sctx);
        let guard = crate::data_engineer::control_flow::DerivedGuardState::default();

        let (reg, card) = DataEngineerSuite::build_tools_for_phase(
            crate::data_engineer::control_flow::Phase::CleansePlan,
            &guard,
            true,
            &sctx,
            None,
            None,
            false,
        )
        .expect("build_tools_for_phase should succeed");

        assert!(!card.contains("ask_user"));
        let err = reg
            .call("ask_user", serde_json::json!({"prompt":"x"}), &actx)
            .await
            .unwrap_err();
        assert!(err.contains("unknown tool"));
    }

    #[tokio::test]
    async fn run_agent_is_non_interactive_on_missing_providers() {
        let sctx = SuiteCtx::default();
        let err = DataEngineerSuite::run_agent("thread-missing-providers", "go", &sctx)
            .await
            .expect_err("agent mode must hard-fail instead of returning an interactive prompt");
        assert!(
            err.contains("warehouse provider configured")
                || err.contains("dbt provider configured")
                || err.contains("catalog bootstrap metadata gate failed")
        );
    }

    #[test]
    fn non_interactive_contract_rejects_await_user_for_agent_type() {
        let frames = vec![FlowFrame::AwaitUser {
            prompt: "x".to_string(),
        }];
        let err = DataEngineerSuite::enforce_non_interactive_contract(AgentMode::Agent, frames)
            .expect_err("agent type must reject AwaitUser");
        assert!(err.contains("agent_mode_await_user_forbidden"));
    }

    #[test]
    fn non_interactive_contract_allows_await_user_for_non_agent_when_not_headless() {
        let frames = vec![FlowFrame::AwaitUser {
            prompt: "x".to_string(),
        }];
        let out = DataEngineerSuite::enforce_non_interactive_contract(AgentMode::Review, frames)
            .expect("non-agent should allow AwaitUser when not headless");
        assert!(matches!(out.first(), Some(FlowFrame::AwaitUser { .. })));
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
            Some(super::AllowedBatch::CleanseSqlDatasetIds(vec![
                "AwsDataCatalog.db.t1".to_string(),
            ])),
            None,
            false,
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
    async fn plan_batched_cleanse_schema_mode_exposes_only_schema_batch_tool() {
        let mut sctx = SuiteCtx::default();
        sctx.query = Some(Arc::new(MockQuery));
        let actx = DataEngineerSuite::agent_tool_ctx("t", &sctx);
        let guard = crate::data_engineer::control_flow::DerivedGuardState::default();
        let (reg, card) = DataEngineerSuite::build_tools_for_phase(
            crate::data_engineer::control_flow::Phase::CleanseAuthor,
            &guard,
            true,
            &sctx,
            Some(super::AllowedBatch::CleanseSchemaDatasetIds(vec![
                "AwsDataCatalog.db.t1".to_string(),
            ])),
            None,
            false,
        )
        .expect("build_tools_for_phase should succeed");
        assert!(card.contains("apply_next_cleanse_schema_batch"));
        assert!(
            !card.contains("- apply_next_cleanse_batch(args:{instructions?:string})"),
            "sql batch tool must not be exposed in schema-next-action mode"
        );
        let err = reg
            .call("apply_next_cleanse_batch", serde_json::json!({}), &actx)
            .await
            .unwrap_err();
        assert!(err.contains("unknown tool"));
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
            Some(super::AllowedBatch::ModelSqlItemNames(vec![
                "fct_orders".to_string()
            ])),
            None,
            false,
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
    async fn plan_batched_model_schema_mode_exposes_only_schema_batch_tool() {
        let mut sctx = SuiteCtx::default();
        sctx.query = Some(Arc::new(MockQuery));
        let actx = DataEngineerSuite::agent_tool_ctx("t", &sctx);
        let guard = crate::data_engineer::control_flow::DerivedGuardState::default();
        let (reg, card) = DataEngineerSuite::build_tools_for_phase(
            crate::data_engineer::control_flow::Phase::ModelAuthor,
            &guard,
            true,
            &sctx,
            Some(super::AllowedBatch::ModelSchemaItemNames(vec![
                "fct_orders".to_string(),
            ])),
            None,
            false,
        )
        .expect("build_tools_for_phase should succeed");
        assert!(card.contains("apply_next_model_schema_batch"));
        assert!(
            !card.contains("- apply_next_model_batch(args:{instructions?:string})"),
            "sql batch tool must not be exposed in schema-next-action mode"
        );
        let err = reg
            .call("apply_next_model_batch", serde_json::json!({}), &actx)
            .await
            .unwrap_err();
        assert!(err.contains("unknown tool"));
    }

    #[tokio::test]
    async fn plan_phase_auto_approves_when_entered_from_review_patch_plan_cleanse() {
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
                implementation_spec: crate::data_engineer::plan::CleanseImplementationSpec {
                    spec_version: 1,
                    row_preserving: true,
                    output_fields: vec![crate::data_engineer::plan::OutputFieldSpec {
                        name: "order_id_raw".to_string(),
                        kind: crate::data_engineer::plan::FieldKind::Raw,
                        source_columns: vec!["order_id".to_string()],
                        expression: "order_id as order_id_raw (raw)".to_string(),
                        data_type: None,
                        nullable: true,
                        description: None,
                    }],
                    prohibited_ops: vec![],
                },
                status: crate::data_engineer::plan::TaskStatus::Pending,
                checklist: crate::data_engineer::plan::canonical_task_checklist(true),
            }],
            batches: vec![vec![ds.clone()]],
            work_groups: crate::data_engineer::plan::canonical_work_groups_from_batches(
                &[vec![ds.clone()]],
                "cleanse",
            ),
            mutations: vec![],
            progress: crate::data_engineer::plan::PlanProgress::default(),
        };
        crate::data_engineer::plan::save_cleanse_plan(&actx, &plan)
            .await
            .expect("save");

        let advanced = DataEngineerSuite::approve_cleanse_plan_draft_and_advance(
            &thread_store,
            thread_id,
            control_flow::Phase::CleansePlan,
            &actx,
            123,
            react_core::control_flow::PhaseReasonCode::PlanAutoApproved,
            serde_json::json!({
                "plan_key": plan_key,
                "entry_reason_code": react_core::control_flow::PhaseReasonCode::ReviewPatchPlan.as_str()
            }),
        )
        .await
        .expect("approve");
        assert!(advanced);

        let loaded = crate::data_engineer::plan::load_cleanse_plan(&actx)
            .await
            .expect("plan");
        assert_eq!(
            loaded.status,
            crate::data_engineer::plan::PlanStatus::Approved
        );
        assert_eq!(loaded.progress.last_applied_step_idx, 123);

        let log2 = thread_store.get(thread_id).await.expect("thread log");
        let last_phase = log2.steps.iter().rev().find_map(|s| match s {
            react_core::session::ThreadStep::Phase {
                phase, reason_code, ..
            } => Some((phase.clone(), reason_code.clone())),
            _ => None,
        });
        let (p, rc) = last_phase.expect("phase");
        assert_eq!(p, control_flow::Phase::CleanseAuthor.as_str());
        assert_eq!(
            rc,
            Some(react_core::control_flow::PhaseReasonCode::PlanAutoApproved)
        );
    }

    #[tokio::test]
    async fn plan_phase_auto_approves_when_entered_from_review_patch_plan_model() {
        let thread_id = "t_auto_model";
        let sctx = SuiteCtx::default();

        // For model plan approval grounding, we need at least one staging model present under models/staging/.
        let base = sctx
            .keyspace
            .dbt_prefix(&sctx.scope)
            .trim_end_matches('/')
            .to_string();
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
                implementation_spec: crate::data_engineer::plan::ModelImplementationSpec {
                    spec_version: 1,
                    grain: "1 row per order".to_string(),
                    inputs: vec!["stg_test_raw_raw_orders".to_string()],
                    joins: vec![],
                    metrics: vec![crate::data_engineer::plan::MetricSpec {
                        name: "orders".to_string(),
                        definition: "count(*)".to_string(),
                        caveats: vec![],
                    }],
                    output_fields: vec![],
                    assumptions: vec![],
                },
                status: crate::data_engineer::plan::TaskStatus::Pending,
                checklist: crate::data_engineer::plan::canonical_task_checklist(false),
            }],
            batches: vec![vec!["fct_orders".to_string()]],
            work_groups: crate::data_engineer::plan::canonical_work_groups_from_batches(
                &[vec!["fct_orders".to_string()]],
                "model",
            ),
            mutations: vec![],
            progress: crate::data_engineer::plan::PlanProgress::default(),
        };
        crate::data_engineer::plan::save_model_plan(&actx, &plan)
            .await
            .expect("save");

        let advanced = DataEngineerSuite::approve_model_plan_draft_and_advance(
            &thread_store,
            thread_id,
            control_flow::Phase::ModelPlan,
            &actx,
            77,
            react_core::control_flow::PhaseReasonCode::PlanAutoApproved,
            serde_json::json!({
                "plan_key": plan_key,
                "entry_reason_code": react_core::control_flow::PhaseReasonCode::ReviewPatchPlan.as_str()
            }),
        )
        .await
        .expect("approve");
        assert!(advanced);

        let loaded = crate::data_engineer::plan::load_model_plan(&actx)
            .await
            .expect("plan");
        assert_eq!(
            loaded.status,
            crate::data_engineer::plan::PlanStatus::Approved
        );
        assert_eq!(loaded.progress.last_applied_step_idx, 77);

        let log2 = thread_store.get(thread_id).await.expect("thread log");
        let last_phase = log2.steps.iter().rev().find_map(|s| match s {
            react_core::session::ThreadStep::Phase {
                phase, reason_code, ..
            } => Some((phase.clone(), reason_code.clone())),
            _ => None,
        });
        let (p, rc) = last_phase.expect("phase");
        assert_eq!(p, control_flow::Phase::ModelAuthor.as_str());
        assert_eq!(
            rc,
            Some(react_core::control_flow::PhaseReasonCode::PlanAutoApproved)
        );
    }

    #[test]
    fn review_question_includes_prior_review_and_mutation_diff_when_available() {
        use crate::data_engineer::control_flow::Phase;
        use crate::data_engineer::progress_controller::{ExecutionState, LastMutationSummary};

        let prior_review_answer = "Please add tests.";
        let mut st = ExecutionState::new();
        st.phase_reason_code = Some(react_core::control_flow::PhaseReasonCode::ReviewPatchPlan);
        st.phase_reason_detail = Some(serde_json::json!({
            "review_phase":"cleanse_review",
            "meta": {"decision":"patch_plan", "dataset_ids": ["x"], "tier":"silver"},
            "answer": prior_review_answer
        }));
        st.last_mutation_summary = Some(LastMutationSummary {
            op: Some("patch".to_string()),
            affected_paths: vec!["models/staging/stg_test_raw_raw_orders.sql".to_string()],
            select_terms: vec!["placed_at_ts".to_string()],
            ts: Some("t".to_string()),
        });

        let q = DataEngineerSuite::build_review_question_with_context(
            "orig goal",
            Phase::CleanseReview,
            &st,
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
            q.contains("Most recent mutation summary"),
            "should include state-based mutation summary"
        );
        assert!(
            q.contains("stg_test_raw_raw_orders.sql"),
            "should include affected path from state"
        );
        assert!(
            q.contains("review_patch_plan"),
            "should include entry reason"
        );
        assert!(
            q.contains("Original goal"),
            "should retain original goal section"
        );
    }

    #[test]
    fn review_question_includes_entry_reason_when_review_started_from_validate_pass() {
        use crate::data_engineer::control_flow::Phase;
        use crate::data_engineer::progress_controller::ExecutionState;

        let mut st = ExecutionState::new();
        st.phase_reason_code = Some(react_core::control_flow::PhaseReasonCode::ValidatePassToReview);
        st.phase_reason_detail = Some(serde_json::json!({"dbt_validate_step_idx": 1}));

        let q = DataEngineerSuite::build_review_question_with_context(
            "orig goal",
            Phase::CleanseReview,
            &st,
        );
        assert!(q.contains("validate_pass_to_review"));
        assert!(q.contains("dbt_validate_step_idx"));
    }

    #[test]
    fn patch_plan_intent_blocks_fast_forward_when_plan_is_unchanged() {
        use crate::data_engineer::control_flow::Phase;
        use crate::data_engineer::progress_controller::{
            ExecutionState, PendingLoopbackIntent,
        };

        let mut st = ExecutionState::new();
        st.pending_loopback_intent = Some(PendingLoopbackIntent::PatchPlan {
            phase: Phase::CleansePlan,
            entry_plan_key: Some("k1".to_string()),
            entry_plan_digest: Some("d1".to_string()),
        });
        assert!(DataEngineerSuite::patch_plan_intent_blocks_fast_forward(
            &st,
            Phase::CleansePlan,
            "k1",
            Some("d1"),
        ));
        assert!(!DataEngineerSuite::patch_plan_intent_blocks_fast_forward(
            &st,
            Phase::CleansePlan,
            "k2",
            Some("d1"),
        ));
        assert!(!DataEngineerSuite::patch_plan_intent_blocks_fast_forward(
            &st,
            Phase::CleansePlan,
            "k1",
            Some("d2"),
        ));
    }

    #[test]
    fn patch_impl_intent_requires_mutation_epoch_advance() {
        use crate::data_engineer::control_flow::Phase;
        use crate::data_engineer::progress_controller::{
            ExecutionState, PendingLoopbackIntent,
        };

        let mut st = ExecutionState::new();
        st.mutation_epoch = 4;
        st.pending_loopback_intent = Some(PendingLoopbackIntent::PatchImpl {
            phase: Phase::ModelAuthor,
            entry_mutation_epoch: 4,
        });
        assert!(DataEngineerSuite::patch_impl_intent_unsatisfied(
            &st,
            Phase::ModelAuthor
        ));
        st.mutation_epoch = 5;
        assert!(!DataEngineerSuite::patch_impl_intent_unsatisfied(
            &st,
            Phase::ModelAuthor
        ));
    }

    #[test]
    fn derive_single_target_repair_path_prefers_execution_state_target() {
        use crate::data_engineer::progress_controller::{ExecutionState, FailedModelRef};
        let mut st = ExecutionState::new();
        st.single_target_repair_path = Some("models/staging/stg_orders.sql".to_string());
        let failed = vec![FailedModelRef {
            name: "stg_other".to_string(),
            file: "models/staging/stg_other.sql".to_string(),
        }];
        let got = DataEngineerSuite::derive_single_target_repair_path(&st, &failed);
        assert_eq!(got, Some("models/staging/stg_orders.sql".to_string()));
    }

    #[test]
    fn derive_single_target_repair_path_falls_back_to_failed_model_file() {
        use crate::data_engineer::progress_controller::{ExecutionState, FailedModelRef};
        let st = ExecutionState::new();
        let failed = vec![FailedModelRef {
            name: "stg_orders".to_string(),
            file: "models/staging/stg_orders.sql".to_string(),
        }];
        let got = DataEngineerSuite::derive_single_target_repair_path(&st, &failed);
        assert_eq!(got, Some("models/staging/stg_orders.sql".to_string()));
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
                    status: react_core::session::ToolStepStatus::Failed,
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

        // Then a successful file patch (even if no-op) should flip patched_since_fail.
        let _ = store
            .append_step(
                tid,
                ThreadStep::ToolEnd {
                    tool_id: "t3".to_string(),
                    name: "file".to_string(),
                    clean_name: "Patch files".to_string(),
                    args: serde_json::json!({"op": "patch"}),
                    status: react_core::session::ToolStepStatus::Ok,
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
            crate::data_engineer::failure_classifier::classify_validate_failure(true, None, None),
            ValidateFailureClass::SchemaOrPrecheck
        );
        // YAML/schema hints should be schema-class.
        assert_eq!(
            crate::data_engineer::failure_classifier::classify_validate_failure(
                false,
                Some("Error in models/schema.yml: duplicate definitions"),
                None
            ),
            ValidateFailureClass::SchemaOrPrecheck
        );
        // Compilation errors should be SQL/runtime-class.
        assert_eq!(
            crate::data_engineer::failure_classifier::classify_validate_failure(
                false,
                Some("Compilation Error: syntax error near FROM"),
                None
            ),
            ValidateFailureClass::SqlOrRuntime
        );
    }

    #[test]
    fn output_field_kind_contract_rejects_unknown_variants() {
        let ok = serde_json::json!({
            "output_fields": [{"name":"a","kind":"raw"},{"name":"b","kind":"quality_flag"}]
        });
        assert!(DataEngineerSuite::validate_output_field_kind_contract(&ok).is_ok());
        let bad = serde_json::json!({
            "output_fields": [{"name":"a","kind":"passthrough"}]
        });
        let err = DataEngineerSuite::validate_output_field_kind_contract(&bad)
            .expect_err("expected invalid kind");
        assert!(err.contains("allowed kind values: raw, clean, derived, quality_flag"));
    }

    #[tokio::test]
    async fn model_plan_can_disable_json_file_after_manifest_retry_suppression() {
        let mut sctx = SuiteCtx::default();
        sctx.query = Some(Arc::new(MockQuery));
        let actx = DataEngineerSuite::agent_tool_ctx("t", &sctx);
        let guard = crate::data_engineer::control_flow::DerivedGuardState::default();
        let (reg, card) = DataEngineerSuite::build_tools_for_phase(
            crate::data_engineer::control_flow::Phase::ModelPlan,
            &guard,
            true,
            &sctx,
            None,
            None,
            true,
        )
        .expect("build_tools_for_phase should succeed");
        let err = reg
            .call(
                "json_file",
                serde_json::json!({"op":"query","path":"target/manifest.json","pointer":"/nodes"}),
                &actx,
            )
            .await
            .expect_err("json_file should be disabled in fallback mode");
        assert!(err.contains("unknown tool"));
        assert!(card.contains("json_file is temporarily disabled"));
    }

    #[test]
    fn model_plan_manifest_retry_state_detects_repeated_failures() {
        use crate::data_engineer::progress_controller::{
            classify_manifest_lookup_failure, classify_manifest_lookup_path, ExecutionState,
        };

        let mut st = ExecutionState::new();
        let path_kind = classify_manifest_lookup_path("manifest.json")
            .expect("manifest.json should classify as manifest lookup path");
        let failure_kind = classify_manifest_lookup_failure(&[
            "not found or failed to fetch: NoSuchKey".to_string(),
        ])
        .expect("NoSuchKey failure should classify");

        st.note_manifest_lookup_attempt(path_kind, false, Some(failure_kind));
        st.note_manifest_lookup_attempt(path_kind, false, Some(failure_kind));

        assert!(st.manifest_lookup.retry_suppressed);
        assert_eq!(st.manifest_lookup.canonical_success_count, 0);
        assert!(
            st.manifest_lookup
                .failure_signature
                .as_deref()
                .unwrap_or("")
                .contains("NoSuchKey"),
            "expected NoSuchKey signature"
        );
    }

    #[test]
    fn run_agent_source_enforces_kernel_transition_and_guard_paths() {
        let src = include_str!("mod.rs");
        let normalized: String = src.chars().filter(|c| !c.is_whitespace()).collect();
        let legacy_transition = ["control_flow::append_phase_with_", "intent", "("].concat();
        let legacy_guard_block = ["ThreadStep::Guard", "Block"].concat();
        assert!(
            !normalized.contains(&legacy_transition),
            "legacy transition path must not appear in mod.rs"
        );
        assert!(
            !normalized.contains(&legacy_guard_block),
            "legacy inline GuardBlock construction must not appear in mod.rs"
        );
        assert!(
            normalized.contains("apply_phase_transition("),
            "kernel transition helper should be used in mod.rs"
        );
        assert!(
            normalized.contains("apply_guard_block("),
            "kernel guard helper should be used in mod.rs"
        );
    }
}
