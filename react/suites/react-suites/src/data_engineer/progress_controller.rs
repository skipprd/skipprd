use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;

use react_core::control_flow::PhaseReasonCode;
use react_core::session::ThreadStore;

use crate::data_engineer::control_flow::Phase;

pub const EXECUTION_STATE_SCHEMA_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FailedModelRef {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub file: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LastValidateState {
    #[serde(default)]
    pub step_idx: Option<usize>,
    #[serde(default)]
    pub ts: Option<String>,
    #[serde(default)]
    pub ok: Option<bool>,
    #[serde(default)]
    pub compile_ok: Option<bool>,
    #[serde(default)]
    pub run_ok: Option<bool>,
    #[serde(default)]
    pub brief: Option<String>,
    #[serde(default)]
    pub failed_models: Vec<FailedModelRef>,
    #[serde(default)]
    pub failure_class: Option<FailureClass>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PublishPlanState {
    #[serde(default)]
    pub pending_plan_sha256: Option<String>,
    #[serde(default)]
    pub pending_set_ts: Option<String>,
    #[serde(default)]
    pub last_published_plan_sha256: Option<String>,
    #[serde(default)]
    pub published_ts: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ArtifactFocusState {
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub dataset_id: Option<String>,
    #[serde(default)]
    pub exists: bool,
    #[serde(default)]
    pub ts: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LastMutationSummary {
    #[serde(default)]
    pub op: Option<String>,
    #[serde(default)]
    pub affected_paths: Vec<String>,
    #[serde(default)]
    pub select_terms: Vec<String>,
    #[serde(default)]
    pub ts: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionTier {
    Unknown,
    Cleanse,
    Model,
}

impl Default for ExecutionTier {
    fn default() -> Self {
        Self::Unknown
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    Discover,
    Mutate,
    Validate,
    Done,
    Failed,
}

impl Default for ExecutionMode {
    fn default() -> Self {
        Self::Discover
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RepairLadderStep {
    PatchTarget = 1,
    ReplaceFile = 2,
    Stop = 3,
}

impl Default for RepairLadderStep {
    fn default() -> Self {
        Self::PatchTarget
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RepairType {
    Unknown,
    Schema,
    SqlTarget,
}

impl Default for RepairType {
    fn default() -> Self {
        Self::Unknown
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProbeOutcomeKind {
    MeaningfulNewSignal,
    MeaningfulSameSignal,
    NonMeaningful,
    Failed,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProbeRequirementStatus {
    NotRequired,
    Required,
    Allowed,
    ExhaustedRequireMutation,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BatchFailureKind {
    SqlValidation,
    InfraTransient,
    SchemaOrContract,
    Unknown,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProbeSignature {
    #[serde(default)]
    pub normalized_sql: String,
    #[serde(default)]
    pub row_count: usize,
    #[serde(default)]
    pub header_count: usize,
    #[serde(default)]
    pub first_row_fingerprint: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProbeState {
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub attempts_total: usize,
    #[serde(default)]
    pub meaningful_attempts: usize,
    #[serde(default)]
    pub repeated_signature_streak: usize,
    #[serde(default)]
    pub non_meaningful_attempts: usize,
    #[serde(default)]
    pub failed_attempts: usize,
    #[serde(default)]
    pub last_signature: Option<ProbeSignature>,
}

impl ProbeSignature {
    pub fn from_run_sql(sql: &str, observation: &Value) -> Self {
        if let Some(p) = observation.get("probe") {
            let normalized_sql = p
                .get("normalized_sql")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    sql.split_whitespace()
                        .collect::<Vec<&str>>()
                        .join(" ")
                        .to_ascii_lowercase()
                });
            let row_count = p
                .get("row_count")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
                .unwrap_or(0);
            let header_count = p
                .get("header_count")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
                .unwrap_or(0);
            let first_row_fingerprint = p
                .get("first_row_fingerprint")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            return Self {
                normalized_sql,
                row_count,
                header_count,
                first_row_fingerprint,
            };
        }
        let normalized_sql = sql
            .split_whitespace()
            .collect::<Vec<&str>>()
            .join(" ")
            .to_ascii_lowercase();
        let header_count = observation
            .get("header")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let rows = observation
            .get("rows")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let row_count = rows.len();
        let first_row_fingerprint = rows.first().and_then(|row| {
            row.as_array().map(|arr| {
                let preview: Vec<&Value> = arr.iter().take(6).collect();
                serde_json::to_string(&preview).unwrap_or_default()
            })
        });
        Self {
            normalized_sql,
            row_count,
            header_count,
            first_row_fingerprint,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RepairTarget {
    #[serde(default)]
    pub model_name: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub error_class: Option<FailureClass>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    WarehouseConfig,
    SqlOrRuntime,
    SchemaOrPrecheck,
    Unknown,
}

impl Default for FailureClass {
    fn default() -> Self {
        Self::Unknown
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FailureSignature {
    pub class: FailureClass,
    #[serde(default)]
    pub node_id: Option<String>,
    #[serde(default)]
    pub canonical_path: Option<String>,
    #[serde(default)]
    pub error_code: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProgressDelta {
    #[serde(default)]
    pub target_hash_changed: bool,
    #[serde(default)]
    pub failed_target_count_delta: i64,
    #[serde(default)]
    pub failure_signature_changed: bool,
    #[serde(default)]
    pub checklist_completed_delta: i64,
    #[serde(default)]
    pub progress_made: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthoringNoProgressReason {
    NoMutationObservedInHardRepair,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthoringProgressSnapshot {
    pub progress_made: bool,
    pub reason: Option<AuthoringNoProgressReason>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SubjectiveRetryKind {
    PlanSemanticInvalid,
    PlanGroundingEmptyAfterPrune,
    ReviewPatchPlan,
    ReviewPatchImpl,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum PublishApprovalDecision {
    AwaitingUserApproval,
    Approved,
    Rejected,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PublishApprovalState {
    pub decision: PublishApprovalDecision,
    pub ts: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum PublishRetryKind {
    AwaitApprovalLoop,
    PublishFailureLoop,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PublishRetryState {
    pub kind: PublishRetryKind,
    pub count: usize,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PendingLoopbackIntent {
    PatchPlan {
        phase: Phase,
        #[serde(default)]
        entry_plan_key: Option<String>,
        #[serde(default)]
        entry_plan_digest: Option<String>,
    },
    PatchImpl {
        phase: Phase,
        entry_mutation_epoch: u64,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SubjectiveRetryState {
    pub phase: Phase,
    pub kind: SubjectiveRetryKind,
    pub count: usize,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ManifestLookupPathKind {
    CanonicalTarget,
    Ambiguous,
    NonCanonical,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ManifestLookupFailureKind {
    NoSuchKey,
    PointerNotFound,
}

impl ManifestLookupFailureKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::NoSuchKey => "NoSuchKey",
            Self::PointerNotFound => "PointerNotFound",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManifestLookupState {
    #[serde(default)]
    pub retry_suppressed: bool,
    #[serde(default)]
    pub failure_signature: Option<String>,
    #[serde(default)]
    pub repeated_failure_count: usize,
    #[serde(default)]
    pub canonical_success_count: usize,
    #[serde(default)]
    pub noncanonical_attempt_count: usize,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionState {
    pub schema_version: u32,
    #[serde(default)]
    pub current_phase: Option<Phase>,
    #[serde(default)]
    pub phase_reason_code: Option<PhaseReasonCode>,
    #[serde(default)]
    pub phase_reason_detail: Option<Value>,
    #[serde(default)]
    pub replan_backtracks: usize,
    #[serde(default)]
    pub last_validate: Option<LastValidateState>,
    #[serde(default)]
    pub current_tier: ExecutionTier,
    #[serde(default)]
    pub mode: ExecutionMode,
    #[serde(default)]
    pub last_validate_ok: Option<bool>,
    #[serde(default)]
    pub last_failure_signature: Option<FailureSignature>,
    #[serde(default)]
    pub repair_backlog: Vec<RepairTarget>,
    #[serde(default)]
    pub hard_mutation_repair_mode: bool,
    #[serde(default)]
    pub repair_type: RepairType,
    #[serde(default)]
    pub single_target_repair_path: Option<String>,
    #[serde(default)]
    pub target_path: Option<String>,
    #[serde(default)]
    pub ladder_step: RepairLadderStep,
    #[serde(default)]
    pub attempt_count: usize,
    #[serde(default)]
    pub mutation_epoch: u64,
    #[serde(default)]
    pub repair_started_mutation_epoch: Option<u64>,
    #[serde(default)]
    pub consecutive_noop_patches: usize,
    #[serde(default)]
    pub stall_count: usize,
    #[serde(default)]
    pub max_stall_count: usize,
    #[serde(default)]
    pub last_progress_delta: Option<ProgressDelta>,
    #[serde(default)]
    pub last_error_brief: Option<String>,
    #[serde(default)]
    pub last_error_class: Option<FailureClass>,
    #[serde(default)]
    pub last_failed_models: Vec<FailedModelRef>,
    #[serde(default)]
    pub subjective_retry: Option<SubjectiveRetryState>,
    #[serde(default)]
    pub publish_approval: Option<PublishApprovalState>,
    #[serde(default)]
    pub publish_retries: Vec<PublishRetryState>,
    #[serde(default)]
    pub pending_loopback_intent: Option<PendingLoopbackIntent>,
    #[serde(default)]
    pub probe_state: ProbeState,
    #[serde(default)]
    pub publish_plan: PublishPlanState,
    #[serde(default)]
    pub artifact_focus: Option<ArtifactFocusState>,
    #[serde(default)]
    pub last_mutation_summary: Option<LastMutationSummary>,
    #[serde(default)]
    pub manifest_lookup: ManifestLookupState,
    #[serde(default)]
    pub cleanse_plan_bootstrap_done: bool,
    #[serde(default)]
    pub model_plan_bootstrap_done: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PhaseState {
    #[serde(default)]
    pub current_phase: Option<Phase>,
    #[serde(default)]
    pub phase_reason_code: Option<PhaseReasonCode>,
    #[serde(default)]
    pub phase_reason_detail: Option<Value>,
    #[serde(default)]
    pub replan_backtracks: usize,
    #[serde(default)]
    pub current_tier: ExecutionTier,
    #[serde(default)]
    pub mode: ExecutionMode,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RepairState {
    #[serde(default)]
    pub last_validate_ok: Option<bool>,
    #[serde(default)]
    pub last_failure_signature: Option<FailureSignature>,
    #[serde(default)]
    pub repair_backlog: Vec<RepairTarget>,
    #[serde(default)]
    pub hard_mutation_repair_mode: bool,
    #[serde(default)]
    pub repair_type: RepairType,
    #[serde(default)]
    pub single_target_repair_path: Option<String>,
    #[serde(default)]
    pub target_path: Option<String>,
    #[serde(default)]
    pub ladder_step: RepairLadderStep,
    #[serde(default)]
    pub attempt_count: usize,
    #[serde(default)]
    pub mutation_epoch: u64,
    #[serde(default)]
    pub repair_started_mutation_epoch: Option<u64>,
    #[serde(default)]
    pub consecutive_noop_patches: usize,
    #[serde(default)]
    pub stall_count: usize,
    #[serde(default)]
    pub max_stall_count: usize,
    #[serde(default)]
    pub last_progress_delta: Option<ProgressDelta>,
    #[serde(default)]
    pub last_error_brief: Option<String>,
    #[serde(default)]
    pub last_error_class: Option<FailureClass>,
    #[serde(default)]
    pub last_failed_models: Vec<FailedModelRef>,
    #[serde(default)]
    pub pending_loopback_intent: Option<PendingLoopbackIntent>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PublishState {
    #[serde(default)]
    pub publish_approval: Option<PublishApprovalState>,
    #[serde(default)]
    pub publish_retries: Vec<PublishRetryState>,
    #[serde(default)]
    pub publish_plan: PublishPlanState,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManifestState {
    #[serde(default)]
    pub manifest_lookup: ManifestLookupState,
    #[serde(default)]
    pub cleanse_plan_bootstrap_done: bool,
    #[serde(default)]
    pub model_plan_bootstrap_done: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DataEngineerEvent {
    ValidatePassed {
        tier: ExecutionTier,
    },
    ValidateFailed {
        tier: ExecutionTier,
        failure_class: FailureClass,
        failure_signature: FailureSignature,
        backlog: Vec<RepairTarget>,
        brief: Option<String>,
        compile_ok: Option<bool>,
        run_ok: Option<bool>,
    },
    BatchAuthoringFailed {
        tier: ExecutionTier,
        kind: BatchFailureKind,
        failed_targets: Vec<FailedModelRef>,
        brief: String,
    },
    BatchAuthoringRecovered,
}

impl ExecutionState {
    pub fn new() -> Self {
        Self {
            schema_version: EXECUTION_STATE_SCHEMA_VERSION,
            max_stall_count: 3,
            ..Default::default()
        }
    }

    pub fn apply_validate_success(&mut self, tier: ExecutionTier) {
        let mut phase = self.phase_state();
        let mut repair = self.repair_state();
        let mut publish = self.publish_state();
        let mut probe: ProbeState;

        phase.current_tier = tier;
        phase.mode = ExecutionMode::Done;
        self.last_validate = Some(LastValidateState {
            ts: Some(chrono::Utc::now().to_rfc3339()),
            ok: Some(true),
            compile_ok: Some(true),
            run_ok: Some(true),
            ..LastValidateState::default()
        });
        repair.last_validate_ok = Some(true);
        repair.last_failure_signature = None;
        repair.repair_backlog.clear();
        repair.hard_mutation_repair_mode = false;
        repair.repair_type = RepairType::Unknown;
        repair.single_target_repair_path = None;
        repair.target_path = None;
        repair.ladder_step = RepairLadderStep::PatchTarget;
        repair.attempt_count = 0;
        repair.repair_started_mutation_epoch = None;
        repair.consecutive_noop_patches = 0;
        repair.stall_count = 0;
        repair.last_progress_delta = Some(ProgressDelta {
            progress_made: true,
            ..ProgressDelta::default()
        });
        repair.last_error_class = None;
        repair.last_failed_models.clear();
        repair.last_error_brief = None;
        self.subjective_retry = None;
        repair.pending_loopback_intent = None;
        publish.publish_approval = None;
        publish.publish_retries.clear();
        probe = ProbeState::default();
        probe.required = false;

        self.set_phase_state(phase);
        self.set_repair_state(repair);
        self.set_publish_state(publish);
        self.set_probe_state_snapshot(probe);
    }

    pub fn phase_state(&self) -> PhaseState {
        PhaseState {
            current_phase: self.current_phase,
            phase_reason_code: self.phase_reason_code,
            phase_reason_detail: self.phase_reason_detail.clone(),
            replan_backtracks: self.replan_backtracks,
            current_tier: self.current_tier.clone(),
            mode: self.mode.clone(),
        }
    }

    pub fn set_phase_state(&mut self, phase: PhaseState) {
        self.current_phase = phase.current_phase;
        self.phase_reason_code = phase.phase_reason_code;
        self.phase_reason_detail = phase.phase_reason_detail;
        self.replan_backtracks = phase.replan_backtracks;
        self.current_tier = phase.current_tier;
        self.mode = phase.mode;
        self.debug_assert_invariants();
    }

    pub fn repair_state(&self) -> RepairState {
        RepairState {
            last_validate_ok: self.last_validate_ok,
            last_failure_signature: self.last_failure_signature.clone(),
            repair_backlog: self.repair_backlog.clone(),
            hard_mutation_repair_mode: self.hard_mutation_repair_mode,
            repair_type: self.repair_type,
            single_target_repair_path: self.single_target_repair_path.clone(),
            target_path: self.target_path.clone(),
            ladder_step: self.ladder_step.clone(),
            attempt_count: self.attempt_count,
            mutation_epoch: self.mutation_epoch,
            repair_started_mutation_epoch: self.repair_started_mutation_epoch,
            consecutive_noop_patches: self.consecutive_noop_patches,
            stall_count: self.stall_count,
            max_stall_count: self.max_stall_count,
            last_progress_delta: self.last_progress_delta.clone(),
            last_error_brief: self.last_error_brief.clone(),
            last_error_class: self.last_error_class,
            last_failed_models: self.last_failed_models.clone(),
            pending_loopback_intent: self.pending_loopback_intent.clone(),
        }
    }

    pub fn set_repair_state(&mut self, repair: RepairState) {
        self.last_validate_ok = repair.last_validate_ok;
        self.last_failure_signature = repair.last_failure_signature;
        self.repair_backlog = repair.repair_backlog;
        self.hard_mutation_repair_mode = repair.hard_mutation_repair_mode;
        self.repair_type = repair.repair_type;
        self.single_target_repair_path = repair.single_target_repair_path;
        self.target_path = repair.target_path;
        self.ladder_step = repair.ladder_step;
        self.attempt_count = repair.attempt_count;
        self.mutation_epoch = repair.mutation_epoch;
        self.repair_started_mutation_epoch = repair.repair_started_mutation_epoch;
        self.consecutive_noop_patches = repair.consecutive_noop_patches;
        self.stall_count = repair.stall_count;
        self.max_stall_count = repair.max_stall_count;
        self.last_progress_delta = repair.last_progress_delta;
        self.last_error_brief = repair.last_error_brief;
        self.last_error_class = repair.last_error_class;
        self.last_failed_models = repair.last_failed_models;
        self.pending_loopback_intent = repair.pending_loopback_intent;
        self.debug_assert_invariants();
    }

    pub fn publish_state(&self) -> PublishState {
        PublishState {
            publish_approval: self.publish_approval.clone(),
            publish_retries: self.publish_retries.clone(),
            publish_plan: self.publish_plan.clone(),
        }
    }

    pub fn set_publish_state(&mut self, publish: PublishState) {
        self.publish_approval = publish.publish_approval;
        self.publish_retries = publish.publish_retries;
        self.publish_plan = publish.publish_plan;
        self.debug_assert_invariants();
    }

    pub fn probe_state_snapshot(&self) -> ProbeState {
        self.probe_state.clone()
    }

    pub fn set_probe_state_snapshot(&mut self, probe: ProbeState) {
        self.probe_state = probe;
        self.debug_assert_invariants();
    }

    pub fn manifest_state(&self) -> ManifestState {
        ManifestState {
            manifest_lookup: self.manifest_lookup.clone(),
            cleanse_plan_bootstrap_done: self.cleanse_plan_bootstrap_done,
            model_plan_bootstrap_done: self.model_plan_bootstrap_done,
        }
    }

    pub fn set_manifest_state(&mut self, manifest: ManifestState) {
        self.manifest_lookup = manifest.manifest_lookup;
        self.cleanse_plan_bootstrap_done = manifest.cleanse_plan_bootstrap_done;
        self.model_plan_bootstrap_done = manifest.model_plan_bootstrap_done;
        self.debug_assert_invariants();
    }

    pub fn reset_manifest_lookup_state(&mut self) {
        let mut manifest = self.manifest_state();
        manifest.manifest_lookup = ManifestLookupState::default();
        self.set_manifest_state(manifest);
    }

    pub fn needs_plan_bootstrap(&self, phase: Phase) -> bool {
        let manifest = self.manifest_state();
        match phase {
            Phase::CleansePlan => !manifest.cleanse_plan_bootstrap_done,
            Phase::ModelPlan => !manifest.model_plan_bootstrap_done,
            _ => false,
        }
    }

    pub fn mark_plan_bootstrap_done(&mut self, phase: Phase) {
        let mut manifest = self.manifest_state();
        match phase {
            Phase::CleansePlan => manifest.cleanse_plan_bootstrap_done = true,
            Phase::ModelPlan => manifest.model_plan_bootstrap_done = true,
            _ => {}
        }
        self.set_manifest_state(manifest);
    }

    pub fn reset_plan_bootstrap(&mut self, phase: Phase) {
        let mut manifest = self.manifest_state();
        match phase {
            Phase::CleansePlan => manifest.cleanse_plan_bootstrap_done = false,
            Phase::ModelPlan => manifest.model_plan_bootstrap_done = false,
            _ => {}
        }
        self.set_manifest_state(manifest);
    }

    pub fn note_manifest_lookup_attempt(
        &mut self,
        path_kind: ManifestLookupPathKind,
        success: bool,
        failure_kind: Option<ManifestLookupFailureKind>,
    ) {
        let mut manifest = self.manifest_state();
        if matches!(
            path_kind,
            ManifestLookupPathKind::Ambiguous | ManifestLookupPathKind::NonCanonical
        ) {
            manifest.manifest_lookup.noncanonical_attempt_count = manifest
                .manifest_lookup
                .noncanonical_attempt_count
                .saturating_add(1);
        }
        if success {
            if path_kind == ManifestLookupPathKind::CanonicalTarget {
                manifest.manifest_lookup.canonical_success_count = manifest
                    .manifest_lookup
                    .canonical_success_count
                    .saturating_add(1);
            }
            manifest.manifest_lookup.retry_suppressed =
                manifest.manifest_lookup.repeated_failure_count >= 2
                    && manifest.manifest_lookup.canonical_success_count == 0;
            self.set_manifest_state(manifest);
            return;
        }
        if let Some(kind) = failure_kind {
            let signature = format!("{}:{path_kind:?}", kind.as_str());
            let repeated = if manifest
                .manifest_lookup
                .failure_signature
                .as_deref()
                .map(|s| s == signature.as_str())
                .unwrap_or(false)
            {
                manifest
                    .manifest_lookup
                    .repeated_failure_count
                    .saturating_add(1)
            } else {
                1
            };
            manifest.manifest_lookup.failure_signature = Some(signature);
            manifest.manifest_lookup.repeated_failure_count = repeated;
        }
        manifest.manifest_lookup.retry_suppressed =
            manifest.manifest_lookup.repeated_failure_count >= 2
                && manifest.manifest_lookup.canonical_success_count == 0;
        self.set_manifest_state(manifest);
    }

    pub fn apply_validate_failure(
        &mut self,
        tier: ExecutionTier,
        failure_class: FailureClass,
        failure_signature: FailureSignature,
        backlog: Vec<RepairTarget>,
        brief: Option<String>,
    ) {
        let mut phase = self.phase_state();
        let mut repair = self.repair_state();
        let mut publish = self.publish_state();
        let mut probe: ProbeState;

        let prev_count = repair.repair_backlog.len() as i64;
        let prev_signature = repair.last_failure_signature.clone();
        phase.current_tier = tier;
        phase.mode = ExecutionMode::Mutate;
        self.last_validate = Some(LastValidateState {
            ts: Some(chrono::Utc::now().to_rfc3339()),
            ok: Some(false),
            compile_ok: Some(
                obs_like_bool(&self.last_validate, |lv| lv.compile_ok)
                    .unwrap_or(false),
            ),
            run_ok: Some(obs_like_bool(&self.last_validate, |lv| lv.run_ok).unwrap_or(false)),
            brief: brief.clone(),
            failed_models: backlog
                .iter()
                .map(|t| FailedModelRef {
                    name: t.model_name.clone().unwrap_or_default(),
                    file: t.path.clone().unwrap_or_default(),
                })
                .collect(),
            failure_class: Some(failure_class),
            ..LastValidateState::default()
        });
        repair.last_validate_ok = Some(false);
        repair.last_failure_signature = Some(failure_signature.clone());
        repair.repair_backlog = backlog;
        repair.hard_mutation_repair_mode = true;
        repair.repair_type = match failure_class {
            FailureClass::SchemaOrPrecheck => RepairType::Schema,
            FailureClass::SqlOrRuntime | FailureClass::WarehouseConfig | FailureClass::Unknown => {
                RepairType::SqlTarget
            }
        };
        repair.single_target_repair_path = repair
            .repair_backlog
            .iter()
            .find_map(|t| t.path.as_ref().map(|s| s.trim().to_string()))
            .filter(|s| !s.is_empty());
        repair.target_path = repair.single_target_repair_path.clone();
        repair.ladder_step = RepairLadderStep::PatchTarget;
        repair.attempt_count = 0;
        repair.repair_started_mutation_epoch = Some(repair.mutation_epoch);
        repair.consecutive_noop_patches = 0;
        repair.last_error_brief = brief;
        repair.last_error_class = Some(failure_class);
        repair.last_failed_models = repair
            .repair_backlog
            .iter()
            .map(|t| FailedModelRef {
                name: t.model_name.clone().unwrap_or_default(),
                file: t.path.clone().unwrap_or_default(),
            })
            .collect();
        publish.publish_approval = None;
        probe = ProbeState::default();
        probe.required = true;
        let compile_ok = self
            .last_validate
            .as_ref()
            .and_then(|v| v.compile_ok)
            .unwrap_or(false);
        probe.required = matches!(failure_class, FailureClass::SqlOrRuntime) && compile_ok;

        let failed_target_count_delta = repair.repair_backlog.len() as i64 - prev_count;
        let failure_signature_changed = prev_signature != Some(failure_signature);
        let progress_made = failed_target_count_delta < 0;
        if progress_made {
            repair.stall_count = 0;
        } else {
            repair.stall_count = repair.stall_count.saturating_add(1);
        }
        repair.last_progress_delta = Some(ProgressDelta {
            target_hash_changed: false,
            failed_target_count_delta,
            failure_signature_changed,
            checklist_completed_delta: 0,
            progress_made,
        });

        self.set_phase_state(phase);
        self.set_repair_state(repair);
        self.set_publish_state(publish);
        self.set_probe_state_snapshot(probe);
    }

    pub fn note_patch_attempt(&mut self, ok: bool, mutated: bool) {
        let mut repair = self.repair_state();
        let mut probe = self.probe_state_snapshot();
        repair.attempt_count = repair.attempt_count.saturating_add(1);
        if ok && mutated {
            repair.consecutive_noop_patches = 0;
            repair.ladder_step = RepairLadderStep::PatchTarget;
            repair.stall_count = 0;
            repair.last_progress_delta = Some(ProgressDelta {
                target_hash_changed: true,
                progress_made: true,
                ..ProgressDelta::default()
            });
            // A successful mutation closes the current probe requirement cycle.
            probe.required = false;
            probe.repeated_signature_streak = 0;
            self.set_repair_state(repair);
            self.set_probe_state_snapshot(probe);
            return;
        }
        repair.consecutive_noop_patches = repair.consecutive_noop_patches.saturating_add(1);
        repair.ladder_step = if repair.attempt_count >= 2 {
            RepairLadderStep::Stop
        } else {
            RepairLadderStep::ReplaceFile
        };
        repair.stall_count = repair.stall_count.saturating_add(1);
        repair.last_progress_delta = Some(ProgressDelta {
            target_hash_changed: false,
            progress_made: false,
            ..ProgressDelta::default()
        });
        self.set_repair_state(repair);
    }

    pub fn reset_probe_state_on_validate(&mut self, is_failure: bool) {
        let mut probe = ProbeState::default();
        probe.required = is_failure;
        self.set_probe_state_snapshot(probe);
    }

    pub fn note_probe_attempt(
        &mut self,
        sql: &str,
        ok: bool,
        signature: ProbeSignature,
    ) -> ProbeOutcomeKind {
        let mut probe = self.probe_state_snapshot();
        probe.attempts_total = probe.attempts_total.saturating_add(1);
        let meaningful_sql = is_meaningful_probe_sql(sql);
        let outcome = if !ok {
            probe.failed_attempts = probe.failed_attempts.saturating_add(1);
            probe.repeated_signature_streak = probe.repeated_signature_streak.saturating_add(1);
            ProbeOutcomeKind::Failed
        } else if !meaningful_sql {
            probe.non_meaningful_attempts = probe.non_meaningful_attempts.saturating_add(1);
            probe.repeated_signature_streak = probe.repeated_signature_streak.saturating_add(1);
            ProbeOutcomeKind::NonMeaningful
        } else if probe.last_signature.as_ref() == Some(&signature) {
            probe.meaningful_attempts = probe.meaningful_attempts.saturating_add(1);
            probe.repeated_signature_streak = probe.repeated_signature_streak.saturating_add(1);
            ProbeOutcomeKind::MeaningfulSameSignal
        } else {
            probe.meaningful_attempts = probe.meaningful_attempts.saturating_add(1);
            probe.repeated_signature_streak = 0;
            ProbeOutcomeKind::MeaningfulNewSignal
        };
        probe.last_signature = Some(signature);
        self.set_probe_state_snapshot(probe);
        outcome
    }

    pub fn probe_requirement_status(&self) -> ProbeRequirementStatus {
        let repair = self.repair_state();
        let probe = self.probe_state_snapshot();
        if repair.last_validate_ok != Some(false) || !probe.required {
            return ProbeRequirementStatus::NotRequired;
        }
        if probe.meaningful_attempts == 0 {
            return ProbeRequirementStatus::Required;
        }
        if probe.repeated_signature_streak >= 3
            || probe.non_meaningful_attempts >= 3
            || probe.failed_attempts >= 3
        {
            return ProbeRequirementStatus::ExhaustedRequireMutation;
        }
        ProbeRequirementStatus::Allowed
    }

    pub fn bump_subjective_retry(
        &mut self,
        phase: Phase,
        kind: SubjectiveRetryKind,
        cap: usize,
    ) -> usize {
        let capped = cap.max(1);
        let next_count = match self.subjective_retry.as_ref() {
            Some(cur) if cur.phase == phase && cur.kind == kind => {
                cur.count.saturating_add(1).min(capped)
            }
            _ => 1.min(capped),
        };
        self.subjective_retry = Some(SubjectiveRetryState {
            phase,
            kind,
            count: next_count,
        });
        next_count
    }

    pub fn reset_subjective_retry(&mut self) {
        self.subjective_retry = None;
    }

    pub fn set_pending_patch_plan_intent(
        &mut self,
        phase: Phase,
        entry_plan_key: Option<String>,
        entry_plan_digest: Option<String>,
    ) {
        let mut repair = self.repair_state();
        repair.pending_loopback_intent = Some(PendingLoopbackIntent::PatchPlan {
            phase,
            entry_plan_key,
            entry_plan_digest,
        });
        self.set_repair_state(repair);
    }

    pub fn set_pending_patch_impl_intent(&mut self, phase: Phase) {
        let mut repair = self.repair_state();
        repair.pending_loopback_intent = Some(PendingLoopbackIntent::PatchImpl {
            phase,
            entry_mutation_epoch: repair.mutation_epoch,
        });
        self.set_repair_state(repair);
    }

    pub fn clear_pending_loopback_intent(&mut self) {
        let mut repair = self.repair_state();
        repair.pending_loopback_intent = None;
        self.set_repair_state(repair);
    }

    pub fn set_publish_approval(&mut self, decision: PublishApprovalDecision) {
        let mut publish = self.publish_state();
        publish.publish_approval = Some(PublishApprovalState {
            decision,
            ts: chrono::Utc::now().to_rfc3339(),
        });
        self.set_publish_state(publish);
    }

    pub fn clear_publish_approval(&mut self) {
        let mut publish = self.publish_state();
        publish.publish_approval = None;
        self.set_publish_state(publish);
    }

    pub fn is_publish_approved(&self) -> bool {
        self.publish_approval
            .as_ref()
            .map(|s| s.decision == PublishApprovalDecision::Approved)
            .unwrap_or(false)
    }

    pub fn bump_publish_retry(&mut self, kind: PublishRetryKind, cap: usize) -> usize {
        let capped = cap.max(1);
        let mut publish = self.publish_state();
        if let Some(existing) = publish.publish_retries.iter_mut().find(|r| r.kind == kind) {
            existing.count = existing.count.saturating_add(1).min(capped);
            let count = existing.count;
            self.set_publish_state(publish);
            return count;
        }
        publish.publish_retries.push(PublishRetryState { kind, count: 1 });
        self.set_publish_state(publish);
        1
    }

    pub fn reset_publish_retry(&mut self, kind: PublishRetryKind) {
        let mut publish = self.publish_state();
        publish.publish_retries.retain(|r| r.kind != kind);
        self.set_publish_state(publish);
    }

    pub fn reset_publish_retries(&mut self) {
        let mut publish = self.publish_state();
        publish.publish_retries.clear();
        self.set_publish_state(publish);
    }

    pub fn enter_validate_mode(&mut self, tier: ExecutionTier) {
        let mut phase = self.phase_state();
        phase.current_tier = tier;
        phase.mode = ExecutionMode::Validate;
        self.set_phase_state(phase);
    }

    pub fn mark_failed(&mut self, brief: impl Into<String>) {
        let mut phase = self.phase_state();
        let mut repair = self.repair_state();
        phase.mode = ExecutionMode::Failed;
        repair.last_error_brief = Some(brief.into());
        self.set_phase_state(phase);
        self.set_repair_state(repair);
    }

    pub async fn load(thread_store: &ThreadStore, thread_id: &str) -> Option<Self> {
        crate::data_engineer::state_manager::load_execution_state(thread_store, thread_id).await
    }

    pub async fn load_strict(
        thread_store: &ThreadStore,
        thread_id: &str,
    ) -> Result<Option<Self>, String> {
        crate::data_engineer::state_manager::load_execution_state_strict(thread_store, thread_id)
            .await
    }

    pub async fn save(&self, thread_store: &ThreadStore, thread_id: &str) -> Result<(), String> {
        crate::data_engineer::state_manager::save_execution_state(thread_store, thread_id, self)
            .await
    }

    pub fn set_pending_publish_plan(&mut self, plan_sha256: String) {
        let mut publish = self.publish_state();
        publish.publish_plan.pending_plan_sha256 = Some(plan_sha256);
        publish.publish_plan.pending_set_ts = Some(chrono::Utc::now().to_rfc3339());
        self.set_publish_state(publish);
    }

    pub fn mark_publish_complete(&mut self, plan_sha256: String) {
        let mut publish = self.publish_state();
        publish.publish_plan.last_published_plan_sha256 = Some(plan_sha256);
        publish.publish_plan.published_ts = Some(chrono::Utc::now().to_rfc3339());
        publish.publish_plan.pending_plan_sha256 = None;
        publish.publish_plan.pending_set_ts = None;
        self.set_publish_state(publish);
    }

    pub fn set_artifact_focus(
        &mut self,
        kind: Option<String>,
        name: Option<String>,
        dataset_id: Option<String>,
        exists: bool,
    ) {
        self.artifact_focus = Some(ArtifactFocusState {
            kind,
            name,
            dataset_id,
            exists,
            ts: Some(chrono::Utc::now().to_rfc3339()),
        });
    }

    pub fn set_last_mutation_summary(
        &mut self,
        op: impl Into<String>,
        affected_paths: Vec<String>,
        select_terms: Vec<String>,
    ) {
        let mut repair = self.repair_state();
        repair.mutation_epoch = repair.mutation_epoch.saturating_add(1);
        self.last_mutation_summary = Some(LastMutationSummary {
            op: Some(op.into()),
            affected_paths,
            select_terms,
            ts: Some(chrono::Utc::now().to_rfc3339()),
        });
        self.set_repair_state(repair);
    }

    pub fn apply_event(&mut self, event: DataEngineerEvent) {
        match event {
            DataEngineerEvent::ValidatePassed { tier } => self.apply_validate_success(tier),
            DataEngineerEvent::ValidateFailed {
                tier,
                failure_class,
                failure_signature,
                backlog,
                brief,
                compile_ok,
                run_ok,
            } => {
                self.apply_validate_failure(
                    tier,
                    failure_class,
                    failure_signature,
                    backlog,
                    brief,
                );
                if let Some(last) = self.last_validate.as_mut() {
                    if let Some(v) = compile_ok {
                        last.compile_ok = Some(v);
                    }
                    if let Some(v) = run_ok {
                        last.run_ok = Some(v);
                    }
                }
            }
            DataEngineerEvent::BatchAuthoringFailed {
                tier,
                kind,
                failed_targets,
                brief,
            } => {
                let failure_class = match kind {
                    BatchFailureKind::SchemaOrContract => FailureClass::SchemaOrPrecheck,
                    BatchFailureKind::SqlValidation => FailureClass::SqlOrRuntime,
                    BatchFailureKind::InfraTransient | BatchFailureKind::Unknown => {
                        FailureClass::Unknown
                    }
                };
                let backlog = repair_backlog_from_failed_models(&failed_targets);
                let sig = FailureSignature {
                    class: failure_class,
                    node_id: failed_targets
                        .first()
                        .map(|f| f.name.trim().to_string())
                        .filter(|s| !s.is_empty()),
                    canonical_path: failed_targets
                        .first()
                        .map(|f| f.file.trim().to_string())
                        .filter(|s| !s.is_empty()),
                    error_code: Some(format!("batch_{:?}", kind).to_ascii_lowercase()),
                };
                self.apply_validate_failure(tier, failure_class, sig, backlog, Some(brief));
            }
            DataEngineerEvent::BatchAuthoringRecovered => {
                let mut repair = self.repair_state();
                repair.hard_mutation_repair_mode = false;
                repair.repair_type = RepairType::Unknown;
                repair.single_target_repair_path = None;
                repair.target_path = None;
                repair.repair_started_mutation_epoch = None;
                repair.last_error_brief = None;
                repair.last_error_class = None;
                repair.last_failed_models.clear();
                repair.pending_loopback_intent = None;
                self.set_repair_state(repair);
            }
        }
        self.debug_assert_invariants();
    }

    pub fn validate_invariants(&self) -> Result<(), String> {
        let mut violations = Vec::new();
        self.collect_phase_coherence_violations(&mut violations);
        self.collect_repair_ladder_coherence_violations(&mut violations);
        self.collect_publish_coherence_violations(&mut violations);
        self.collect_probe_lifecycle_violations(&mut violations);
        if violations.is_empty() {
            return Ok(());
        }
        Err(format!(
            "execution_state invariant violation(s): {}",
            violations.join("; ")
        ))
    }

    fn collect_phase_coherence_violations(&self, violations: &mut Vec<String>) {
        let phase = self.phase_state();
        if phase.phase_reason_code.is_some() && phase.current_phase.is_none() {
            violations.push("phase_reason_code set while current_phase is none".to_string());
        }
        if phase.phase_reason_detail.is_some() && phase.current_phase.is_none() {
            violations.push("phase_reason_detail set while current_phase is none".to_string());
        }
    }

    fn collect_repair_ladder_coherence_violations(&self, violations: &mut Vec<String>) {
        let repair = self.repair_state();
        if repair.ladder_step != RepairLadderStep::PatchTarget && !repair.hard_mutation_repair_mode {
            violations.push(
                "repair ladder advanced while hard_mutation_repair_mode is disabled".to_string(),
            );
        }
        if repair.ladder_step == RepairLadderStep::Stop && repair.attempt_count < 2 {
            violations.push("repair ladder reached stop before two attempts".to_string());
        }
        if let Some(single_target) = repair
            .single_target_repair_path
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let target = repair.target_path.as_deref().map(str::trim);
            if target != Some(single_target) {
                violations.push(
                    "single_target_repair_path must match target_path when present".to_string(),
                );
            }
        }
        if repair.hard_mutation_repair_mode && repair.repair_type == RepairType::Unknown {
            violations.push("hard_mutation_repair_mode requires non-unknown repair_type".to_string());
        }
    }

    fn collect_publish_coherence_violations(&self, violations: &mut Vec<String>) {
        let publish = self.publish_state();
        if let Some(approval) = publish.publish_approval.as_ref() {
            if approval.ts.trim().is_empty() {
                violations.push("publish_approval timestamp must be non-empty".to_string());
            }
        }
        let mut kinds = HashSet::new();
        for retry in &publish.publish_retries {
            if retry.count == 0 {
                violations.push(format!("publish retry {:?} has zero count", retry.kind));
            }
            if !kinds.insert(retry.kind) {
                violations.push(format!("duplicate publish retry entry for {:?}", retry.kind));
            }
        }
    }

    fn collect_probe_lifecycle_violations(&self, violations: &mut Vec<String>) {
        let repair = self.repair_state();
        let probe = self.probe_state_snapshot();
        if probe.required && repair.last_validate_ok != Some(false) {
            violations.push(
                "probe.required can only be true while last_validate_ok is false".to_string(),
            );
        }
        let classified_attempts = probe
            .meaningful_attempts
            .saturating_add(probe.non_meaningful_attempts)
            .saturating_add(probe.failed_attempts);
        if classified_attempts > probe.attempts_total {
            violations.push(
                "probe attempt counters exceed attempts_total".to_string(),
            );
        }
        if probe.repeated_signature_streak > probe.attempts_total {
            violations.push(
                "probe repeated_signature_streak exceeds attempts_total".to_string(),
            );
        }
    }

    fn debug_assert_invariants(&self) {
        debug_assert!(
            self.validate_invariants().is_ok(),
            "invalid execution state: {:?}",
            self.validate_invariants()
        );
    }
}

pub fn normalize_manifest_path(path: &str) -> String {
    path.trim().trim_matches('/').replace('\\', "/")
}

pub fn classify_manifest_lookup_path(path: &str) -> Option<ManifestLookupPathKind> {
    let norm = normalize_manifest_path(path);
    if norm.is_empty() {
        return None;
    }
    if norm == "target/manifest.json" {
        return Some(ManifestLookupPathKind::CanonicalTarget);
    }
    if norm.ends_with("target/manifest.json") {
        return Some(ManifestLookupPathKind::NonCanonical);
    }
    if norm.ends_with("manifest.json") {
        return Some(ManifestLookupPathKind::Ambiguous);
    }
    None
}

pub fn classify_manifest_lookup_failure(errors: &[String]) -> Option<ManifestLookupFailureKind> {
    let joined = errors.join("\n").to_ascii_lowercase();
    if joined.contains("nosuchkey")
        || joined.contains("not found or failed to fetch")
        || joined.contains("the specified key does not exist")
    {
        return Some(ManifestLookupFailureKind::NoSuchKey);
    }
    if joined.contains("pointer not found") {
        return Some(ManifestLookupFailureKind::PointerNotFound);
    }
    None
}

fn obs_like_bool(
    from: &Option<LastValidateState>,
    pick: impl FnOnce(&LastValidateState) -> Option<bool>,
) -> Option<bool> {
    from.as_ref().and_then(pick)
}

pub fn repair_backlog_from_failed_models(failing_models: &[FailedModelRef]) -> Vec<RepairTarget> {
    let mut out: Vec<RepairTarget> = failing_models
        .iter()
        .map(|fm| RepairTarget {
            model_name: Some(fm.name.trim().to_string()).filter(|s| !s.is_empty()),
            path: Some(fm.file.trim().to_string()).filter(|s| !s.is_empty() && s != "(unknown file)"),
            error_class: None,
        })
        .collect();
    out.sort_by(|a, b| a.path.cmp(&b.path).then(a.model_name.cmp(&b.model_name)));
    out.dedup_by(|a, b| a.path == b.path && a.model_name == b.model_name);
    out
}

pub fn failed_model_refs_from_values(values: &[Value]) -> Vec<FailedModelRef> {
    values
        .iter()
        .map(|v| FailedModelRef {
            name: v
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .trim()
                .to_string(),
            file: v
                .get("file")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .trim()
                .to_string(),
        })
        .collect()
}

pub fn gate_authoring_progress(state: &ExecutionState, phase: Phase) -> Result<(), String> {
    let last_validate_failed = state.last_validate_ok == Some(false);
    let mutation_progress = state
        .last_progress_delta
        .as_ref()
        .map(|d| d.progress_made || d.target_hash_changed)
        .unwrap_or(false);
    let patched_since_fail = state.attempt_count > 0;
    if last_validate_failed && !(mutation_progress || patched_since_fail) {
        return Err(
            "progress_gate_blocked: validation previously failed and no successful mutation has been recorded since that failure"
                .to_string(),
        );
    }
    match state.probe_requirement_status() {
        ProbeRequirementStatus::Required => {
            return Err(
                "progress_gate_blocked: runtime validation previously failed after compile and a meaningful data probe is still required"
                    .to_string(),
            );
        }
        ProbeRequirementStatus::ExhaustedRequireMutation => {
            return Err(
                "progress_gate_blocked: probe loop exhausted (repeated/no-new-signal probes); apply a mutating fix before validating"
                    .to_string(),
            );
        }
        ProbeRequirementStatus::NotRequired | ProbeRequirementStatus::Allowed => {}
    }
    // Keep existing unresolved mutation-failure behavior, but as a deterministic progress gate.
    match phase {
        Phase::CleanseAuthor | Phase::ModelAuthor => {}
        _ => return Ok(()),
    }
    Ok(())
}

pub fn snapshot_authoring_stepboundary_progress(
    hard_mutation_repair_mode: bool,
    last_validate_failed: bool,
    pre_mutation_epoch: u64,
    post_mutation_epoch: u64,
    post_stall_count: usize,
    max_stall_count: usize,
) -> AuthoringProgressSnapshot {
    if hard_mutation_repair_mode
        && last_validate_failed
        && post_mutation_epoch <= pre_mutation_epoch
        && post_stall_count >= max_stall_count.max(1)
    {
        return AuthoringProgressSnapshot {
            progress_made: false,
            reason: Some(AuthoringNoProgressReason::NoMutationObservedInHardRepair),
        };
    }
    AuthoringProgressSnapshot {
        progress_made: true,
        reason: None,
    }
}

pub fn is_meaningful_probe_sql(sql: &str) -> bool {
    let s = sql.trim().trim_end_matches(';').trim().to_lowercase();
    if s.is_empty() {
        return false;
    }
    let toks: Vec<&str> = s.split_whitespace().collect();
    if toks == ["select", "1"] {
        return false;
    }
    if toks.len() == 4 && toks[0] == "select" && toks[1] == "1" && toks[2] == "as" {
        return false;
    }
    toks.iter().any(|t| *t == "from")
}

pub fn gate_publish_progress(state: &ExecutionState, phase: Phase) -> Result<(), String> {
    match phase {
        Phase::PublishAwaitApproval | Phase::Publish => {}
        _ => return Ok(()),
    }
    if state.is_publish_approved() {
        return Ok(());
    }
    Err("publish_gate_blocked: publish requires explicit persisted approval state".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use react_core::keyspace::DefaultKeyspace;
    use react_core::scope::RequestScope;
    use react_core::session::ThreadStore;
    use react_core::storage::InMemoryStorageAdapter;
    use std::sync::Arc;

    #[test]
    fn state_has_compile_time_defaults() {
        let st = ExecutionState::new();
        assert_eq!(st.current_tier, ExecutionTier::Unknown);
        assert_eq!(st.mode, ExecutionMode::Discover);
    }

    #[test]
    fn authoring_stepboundary_snapshot_flags_hard_repair_no_progress() {
        let snapshot = snapshot_authoring_stepboundary_progress(
            true,
            true,
            4,
            4,
            3,
            3,
        );
        assert!(!snapshot.progress_made);
        assert_eq!(
            snapshot.reason,
            Some(AuthoringNoProgressReason::NoMutationObservedInHardRepair)
        );
    }

    #[test]
    fn authoring_stepboundary_snapshot_accepts_mutation_progress() {
        let snapshot = snapshot_authoring_stepboundary_progress(
            true,
            true,
            4,
            5,
            0,
            3,
        );
        assert!(snapshot.progress_made);
        assert_eq!(snapshot.reason, None);
    }

    #[test]
    fn authoring_stepboundary_snapshot_allows_single_non_mutating_turn_before_budget() {
        let snapshot = snapshot_authoring_stepboundary_progress(
            true,
            true,
            5,
            5,
            1,
            3,
        );
        assert!(snapshot.progress_made);
        assert_eq!(snapshot.reason, None);
    }

    #[test]
    fn validate_success_resets_repair_and_retry_state() {
        let mut st = ExecutionState::new();
        st.hard_mutation_repair_mode = true;
        st.repair_type = RepairType::SqlTarget;
        st.subjective_retry = Some(SubjectiveRetryState {
            phase: Phase::ModelPlan,
            kind: SubjectiveRetryKind::PlanSemanticInvalid,
            count: 3,
        });
        st.apply_validate_success(ExecutionTier::Model);
        assert_eq!(st.current_tier, ExecutionTier::Model);
        assert_eq!(st.mode, ExecutionMode::Done);
        assert!(!st.hard_mutation_repair_mode);
        assert_eq!(st.repair_type, RepairType::Unknown);
        assert!(st.subjective_retry.is_none());
    }

    #[test]
    fn validate_failure_sets_typed_repair_type() {
        let mut st = ExecutionState::new();
        let sig = FailureSignature {
            class: FailureClass::SchemaOrPrecheck,
            ..FailureSignature::default()
        };
        st.apply_validate_failure(
            ExecutionTier::Cleanse,
            FailureClass::SchemaOrPrecheck,
            sig,
            Vec::new(),
            Some("schema fail".to_string()),
        );
        assert_eq!(st.repair_type, RepairType::Schema);

        let sig2 = FailureSignature {
            class: FailureClass::SqlOrRuntime,
            ..FailureSignature::default()
        };
        st.apply_validate_failure(
            ExecutionTier::Cleanse,
            FailureClass::SqlOrRuntime,
            sig2,
            Vec::new(),
            Some("sql fail".to_string()),
        );
        assert_eq!(st.repair_type, RepairType::SqlTarget);
    }

    #[test]
    fn subjective_retry_is_single_state_and_bounded() {
        let mut st = ExecutionState::new();
        assert_eq!(
            st.bump_subjective_retry(Phase::CleansePlan, SubjectiveRetryKind::PlanSemanticInvalid, 3),
            1
        );
        assert_eq!(
            st.bump_subjective_retry(Phase::CleansePlan, SubjectiveRetryKind::PlanSemanticInvalid, 3),
            2
        );
        assert_eq!(
            st.bump_subjective_retry(Phase::CleansePlan, SubjectiveRetryKind::PlanSemanticInvalid, 3),
            3
        );
        assert_eq!(
            st.bump_subjective_retry(Phase::CleansePlan, SubjectiveRetryKind::PlanSemanticInvalid, 3),
            3
        );
        assert_eq!(
            st.bump_subjective_retry(
                Phase::ModelPlan,
                SubjectiveRetryKind::PlanGroundingEmptyAfterPrune,
                3
            ),
            1
        );
    }

    #[test]
    fn gate_authoring_progress_uses_execution_state() {
        let mut st = ExecutionState::new();
        st.last_validate_ok = Some(false);
        st.last_validate = Some(LastValidateState {
            compile_ok: Some(true),
            run_ok: Some(false),
            ..LastValidateState::default()
        });
        st.probe_state.required = true;
        assert!(gate_authoring_progress(&st, Phase::ModelAuthor).is_err());

        st.attempt_count = 1;
        st.last_progress_delta = Some(ProgressDelta {
            target_hash_changed: true,
            progress_made: true,
            ..ProgressDelta::default()
        });
        st.last_validate = Some(LastValidateState {
            compile_ok: Some(true),
            run_ok: Some(true),
            ..LastValidateState::default()
        });
        st.probe_state.required = false;
        assert!(gate_authoring_progress(&st, Phase::ModelAuthor).is_ok());
    }

    #[test]
    fn probe_status_allows_multiple_meaningful_probes_and_exhausts_on_repeats() {
        let mut st = ExecutionState::new();
        st.last_validate_ok = Some(false);
        st.probe_state.required = true;

        let sig1 =
            ProbeSignature::from_run_sql("select * from x limit 10", &serde_json::json!({"ok":true}));
        let out1 = st.note_probe_attempt("select * from x limit 10", true, sig1.clone());
        assert_eq!(out1, ProbeOutcomeKind::MeaningfulNewSignal);
        assert_eq!(st.probe_requirement_status(), ProbeRequirementStatus::Allowed);

        let sig2 =
            ProbeSignature::from_run_sql("select * from y limit 10", &serde_json::json!({"ok":true}));
        let out2 = st.note_probe_attempt("select * from y limit 10", true, sig2);
        assert_eq!(out2, ProbeOutcomeKind::MeaningfulNewSignal);
        assert_eq!(st.probe_requirement_status(), ProbeRequirementStatus::Allowed);

        let _ = st.note_probe_attempt("select * from x limit 10", true, sig1.clone());
        let _ = st.note_probe_attempt("select * from x limit 10", true, sig1.clone());
        let _ = st.note_probe_attempt("select * from x limit 10", true, sig1);
        let _ = st.note_probe_attempt(
            "select * from x limit 10",
            true,
            ProbeSignature::from_run_sql("select * from x limit 10", &serde_json::json!({"ok":true})),
        );
        assert_eq!(
            st.probe_requirement_status(),
            ProbeRequirementStatus::ExhaustedRequireMutation
        );
    }

    #[test]
    fn successful_mutation_resets_probe_requirement_cycle() {
        let mut st = ExecutionState::new();
        st.last_validate_ok = Some(false);
        st.probe_state.required = true;
        st.note_patch_attempt(true, true);
        assert_eq!(st.probe_requirement_status(), ProbeRequirementStatus::NotRequired);
    }

    #[test]
    fn validate_and_failed_modes_are_set_via_controller_helpers() {
        let mut st = ExecutionState::new();
        st.enter_validate_mode(ExecutionTier::Cleanse);
        assert_eq!(st.mode, ExecutionMode::Validate);
        assert_eq!(st.current_tier, ExecutionTier::Cleanse);
        st.mark_failed("x");
        assert_eq!(st.mode, ExecutionMode::Failed);
        assert_eq!(st.last_error_brief.as_deref(), Some("x"));
    }

    #[test]
    fn publish_gate_requires_explicit_approval() {
        let mut st = ExecutionState::new();
        assert!(gate_publish_progress(&st, Phase::Publish).is_err());
        st.set_publish_approval(PublishApprovalDecision::Approved);
        assert!(gate_publish_progress(&st, Phase::PublishAwaitApproval).is_ok());
        assert!(gate_publish_progress(&st, Phase::Publish).is_ok());
        st.set_publish_approval(PublishApprovalDecision::Rejected);
        assert!(gate_publish_progress(&st, Phase::Publish).is_err());
    }

    #[test]
    fn publish_retry_budget_is_tracked_by_typed_kind() {
        let mut st = ExecutionState::new();
        assert_eq!(
            st.bump_publish_retry(PublishRetryKind::AwaitApprovalLoop, 3),
            1
        );
        assert_eq!(
            st.bump_publish_retry(PublishRetryKind::AwaitApprovalLoop, 3),
            2
        );
        assert_eq!(
            st.bump_publish_retry(PublishRetryKind::PublishFailureLoop, 3),
            1
        );
        st.reset_publish_retry(PublishRetryKind::AwaitApprovalLoop);
        assert_eq!(
            st.publish_retries
                .iter()
                .find(|r| r.kind == PublishRetryKind::AwaitApprovalLoop)
                .map(|r| r.count),
            None
        );
    }

    #[test]
    fn repeated_equivalent_validate_failures_do_not_count_as_progress() {
        let mut st = ExecutionState::new();
        let sig = FailureSignature {
            class: FailureClass::SqlOrRuntime,
            node_id: Some("model.pkg.fct_orders".to_string()),
            canonical_path: Some("models/marts/fct_orders.sql".to_string()),
            error_code: Some("E_SQL".to_string()),
        };
        let backlog = vec![RepairTarget {
            model_name: Some("model.pkg.fct_orders".to_string()),
            path: Some("models/marts/fct_orders.sql".to_string()),
            error_class: Some(FailureClass::SqlOrRuntime),
        }];

        st.apply_validate_failure(
            ExecutionTier::Model,
            FailureClass::SqlOrRuntime,
            sig.clone(),
            backlog.clone(),
            Some("first".to_string()),
        );
        let stall_after_first = st.stall_count;

        st.apply_validate_failure(
            ExecutionTier::Model,
            FailureClass::SqlOrRuntime,
            sig,
            backlog,
            Some("second".to_string()),
        );

        let delta = st.last_progress_delta.expect("delta");
        assert!(!delta.progress_made);
        assert_eq!(delta.failed_target_count_delta, 0);
        assert!(!delta.failure_signature_changed);
        assert_eq!(st.stall_count, stall_after_first.saturating_add(1));
    }

    #[test]
    fn shrinking_repair_backlog_counts_as_structural_progress() {
        let mut st = ExecutionState::new();
        let sig = FailureSignature {
            class: FailureClass::SqlOrRuntime,
            node_id: Some("model.pkg.fct_orders".to_string()),
            canonical_path: Some("models/marts/fct_orders.sql".to_string()),
            error_code: Some("E_SQL".to_string()),
        };
        let two = vec![
            RepairTarget {
                model_name: Some("model.pkg.fct_orders".to_string()),
                path: Some("models/marts/fct_orders.sql".to_string()),
                error_class: Some(FailureClass::SqlOrRuntime),
            },
            RepairTarget {
                model_name: Some("model.pkg.dim_users".to_string()),
                path: Some("models/marts/dim_users.sql".to_string()),
                error_class: Some(FailureClass::SqlOrRuntime),
            },
        ];
        st.apply_validate_failure(
            ExecutionTier::Model,
            FailureClass::SqlOrRuntime,
            sig.clone(),
            two,
            Some("first".to_string()),
        );

        let one = vec![RepairTarget {
            model_name: Some("model.pkg.fct_orders".to_string()),
            path: Some("models/marts/fct_orders.sql".to_string()),
            error_class: Some(FailureClass::SqlOrRuntime),
        }];
        st.apply_validate_failure(
            ExecutionTier::Model,
            FailureClass::SqlOrRuntime,
            sig,
            one,
            Some("second".to_string()),
        );

        let delta = st.last_progress_delta.expect("delta");
        assert!(delta.progress_made);
        assert_eq!(delta.failed_target_count_delta, -1);
    }

    #[test]
    fn apply_event_batch_authoring_failed_enters_hard_repair_mode() {
        let mut st = ExecutionState::new();
        st.apply_event(DataEngineerEvent::BatchAuthoringFailed {
            tier: ExecutionTier::Cleanse,
            kind: BatchFailureKind::SqlValidation,
            failed_targets: vec![FailedModelRef {
                name: "AwsDataCatalog.test_raw.raw_customers".to_string(),
                file: "models/staging/stg_test_raw_raw_customers.sql".to_string(),
            }],
            brief: "sql validation failed".to_string(),
        });
        assert!(st.hard_mutation_repair_mode);
        assert_eq!(st.repair_type, RepairType::SqlTarget);
        assert_eq!(
            st.single_target_repair_path.as_deref(),
            Some("models/staging/stg_test_raw_raw_customers.sql")
        );
        assert_eq!(st.last_validate_ok, Some(false));
    }

    #[test]
    fn apply_event_batch_authoring_recovered_clears_repair_mode() {
        let mut st = ExecutionState::new();
        st.hard_mutation_repair_mode = true;
        st.repair_type = RepairType::SqlTarget;
        st.single_target_repair_path = Some("models/staging/x.sql".to_string());
        st.target_path = Some("models/staging/x.sql".to_string());
        st.last_failed_models = vec![FailedModelRef {
            name: "x".to_string(),
            file: "models/staging/x.sql".to_string(),
        }];
        st.apply_event(DataEngineerEvent::BatchAuthoringRecovered);
        assert!(!st.hard_mutation_repair_mode);
        assert_eq!(st.repair_type, RepairType::Unknown);
        assert!(st.single_target_repair_path.is_none());
        assert!(st.target_path.is_none());
        assert!(st.last_failed_models.is_empty());
    }

    #[test]
    fn invariants_reject_phase_reason_without_current_phase() {
        let mut st = ExecutionState::new();
        st.current_phase = None;
        st.phase_reason_code = Some(PhaseReasonCode::PhaseSet);
        let err = st.validate_invariants().expect_err("invariants must fail");
        assert!(err.contains("phase_reason_code set while current_phase is none"));
    }

    #[test]
    fn invariants_reject_repair_ladder_stop_without_required_attempts() {
        let mut st = ExecutionState::new();
        st.hard_mutation_repair_mode = true;
        st.repair_type = RepairType::SqlTarget;
        st.ladder_step = RepairLadderStep::Stop;
        st.attempt_count = 1;
        let err = st.validate_invariants().expect_err("invariants must fail");
        assert!(err.contains("repair ladder reached stop before two attempts"));
    }

    #[test]
    fn invariants_reject_duplicate_publish_retry_entries() {
        let mut st = ExecutionState::new();
        st.publish_retries = vec![
            PublishRetryState {
                kind: PublishRetryKind::AwaitApprovalLoop,
                count: 1,
            },
            PublishRetryState {
                kind: PublishRetryKind::AwaitApprovalLoop,
                count: 2,
            },
        ];
        let err = st.validate_invariants().expect_err("invariants must fail");
        assert!(err.contains("duplicate publish retry entry"));
    }

    #[test]
    fn invariants_reject_probe_required_when_last_validate_not_failed() {
        let mut st = ExecutionState::new();
        st.last_validate_ok = Some(true);
        st.probe_state.required = true;
        let err = st.validate_invariants().expect_err("invariants must fail");
        assert!(err.contains("probe.required can only be true"));
    }

    #[tokio::test]
    async fn load_strict_rejects_malformed_control_state() {
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let scope = RequestScope {
            tenant: "t".into(),
            workspace: "w".into(),
            project_id: "p".into(),
        };
        let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
        let store = ThreadStore::new(storage, scope, keyspace);
        let tid = "tid-malformed-control-state";

        let mut st = react_core::session::ThreadState {
            thread_state_schema_version: react_core::session::THREAD_STATE_SCHEMA_VERSION,
            thread_id: tid.to_string(),
            ..react_core::session::ThreadState::default()
        };
        st.control_state = Some(serde_json::json!({"schema_version":"bad"}));
        store
            .put_thread_state(tid, &st)
            .await
            .expect("seed thread state");

        let got = ExecutionState::load_strict(&store, tid).await;
        assert!(got.is_err(), "malformed control_state must fail loudly");
    }
}
