use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Minimal, strongly-typed control-flow primitives shared across suites and the WS/UI layer.
///
/// This is intentionally KISS:
/// - Enums capture *decisions* and *reasons* (finite sets) for compile-time exhaustiveness.
/// - Free-form detail remains `serde_json::Value` (debug-only) and must not drive behavior.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    /// No action required; proceed forward in the deterministic pipeline.
    Proceed,
    /// The plan/spec is wrong or ambiguous; return to planning.
    PatchPlan,
    /// The implementation deviates from the approved plan/spec; return to authoring.
    PatchImpl,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewTier {
    Silver,
    Gold,
    Unknown,
}

impl Default for ReviewTier {
    fn default() -> Self {
        ReviewTier::Unknown
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewDecisionMeta {
    pub decision: ReviewDecision,
    #[serde(default)]
    pub tier: ReviewTier,
    #[serde(default)]
    pub dataset_ids: Vec<String>,
    /// Optional stable reference to the persisted review text (suite-defined key).
    #[serde(default)]
    pub review_ref: Option<Value>,
}

/// Reason codes for phase transitions recorded in thread logs.
///
/// These are intended for UI/debuggability and (critically) for suites that want to
/// branch deterministically on the most recent phase entry reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseReasonCode {
    // Generic
    PhaseSet,

    // Preflight
    PreflightStart,
    PreflightOk,

    // Planning
    PlanApproved,
    PlanAutoApproved,
    PlanAlreadyApproved,
    PlanMissing,
    PlanNotApproved,
    PlanInvalidEmpty,
    PlanPrunedEmpty,
    PlanSemanticInvalid,

    // Execution / workgroups
    WorkGroupValidate,
    PlanTasksDone,
    NoWorkAllDone,

    // Authoring / validate
    AuthoringComplete,
    PrecheckFailed,
    ValidatePass,
    ValidateFail,

    // Review decisions
    ReviewProceed,
    ReviewPatchPlan,
    ReviewPatchImpl,

    // Review snapshot markers (non-transition, same-phase annotations)
    ReviewProjectSummary,
    ReviewBatch,
    ReviewFinalUnify,

    // Publish
    UserApprovedPublish,
    PublishSuccess,
    PublishFail,
    PublishConfirmedSuccess,
    PublishConfirmedFail,

    // Blocking / guards
    PhaseBlocked,
}

impl PhaseReasonCode {
    pub fn as_str(self) -> &'static str {
        match self {
            PhaseReasonCode::PhaseSet => "phase_set",
            PhaseReasonCode::PreflightStart => "preflight_start",
            PhaseReasonCode::PreflightOk => "preflight_ok",
            PhaseReasonCode::PlanApproved => "plan_approved",
            PhaseReasonCode::PlanAutoApproved => "plan_auto_approved",
            PhaseReasonCode::PlanAlreadyApproved => "plan_already_approved",
            PhaseReasonCode::PlanMissing => "plan_missing",
            PhaseReasonCode::PlanNotApproved => "plan_not_approved",
            PhaseReasonCode::PlanInvalidEmpty => "plan_invalid_empty",
            PhaseReasonCode::PlanPrunedEmpty => "plan_pruned_empty",
            PhaseReasonCode::PlanSemanticInvalid => "plan_semantic_invalid",
            PhaseReasonCode::WorkGroupValidate => "work_group_validate",
            PhaseReasonCode::PlanTasksDone => "plan_tasks_done",
            PhaseReasonCode::NoWorkAllDone => "no_work_all_done",
            PhaseReasonCode::AuthoringComplete => "authoring_complete",
            PhaseReasonCode::PrecheckFailed => "precheck_failed",
            PhaseReasonCode::ValidatePass => "validate_pass",
            PhaseReasonCode::ValidateFail => "validate_fail",
            PhaseReasonCode::ReviewProceed => "review_proceed",
            PhaseReasonCode::ReviewPatchPlan => "review_patch_plan",
            PhaseReasonCode::ReviewPatchImpl => "review_patch_impl",
            PhaseReasonCode::ReviewProjectSummary => "review_project_summary",
            PhaseReasonCode::ReviewBatch => "review_batch",
            PhaseReasonCode::ReviewFinalUnify => "review_final_unify",
            PhaseReasonCode::UserApprovedPublish => "user_approved_publish",
            PhaseReasonCode::PublishSuccess => "publish_success",
            PhaseReasonCode::PublishFail => "publish_fail",
            PhaseReasonCode::PublishConfirmedSuccess => "publish_confirmed_success",
            PhaseReasonCode::PublishConfirmedFail => "publish_confirmed_fail",
            PhaseReasonCode::PhaseBlocked => "phase_blocked",
        }
    }
}

/// Guard block categories recorded in the thread log/state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardBlockKind {
    PlanJsonInvalid,
    PlanGrounding,
    PlanSemanticInvalid,
    PlanDesignCritique,
    BatchLocked,
    AuthoringCompletion,
    AuthoringToValidate,
    MissingGoldModels,
    PrecheckFailed,
    MissingThreadStep,
}

impl GuardBlockKind {
    pub fn as_str(self) -> &'static str {
        match self {
            GuardBlockKind::PlanJsonInvalid => "plan_json_invalid",
            GuardBlockKind::PlanGrounding => "plan_grounding",
            GuardBlockKind::PlanSemanticInvalid => "plan_semantic_invalid",
            GuardBlockKind::PlanDesignCritique => "plan_design_critique",
            GuardBlockKind::BatchLocked => "batch_locked",
            GuardBlockKind::AuthoringCompletion => "authoring_completion",
            GuardBlockKind::AuthoringToValidate => "authoring_to_validate",
            GuardBlockKind::MissingGoldModels => "missing_gold_models",
            GuardBlockKind::PrecheckFailed => "precheck_failed",
            GuardBlockKind::MissingThreadStep => "missing_thread_step",
        }
    }
}
