use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain_types::{ReviewDecision, ReviewDecisionMeta};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoApprovalSource {
    NewDraftPlan,
    ExistingDraftPlan,
}

impl std::fmt::Display for AutoApprovalSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NewDraftPlan => f.write_str("new_draft_plan"),
            Self::ExistingDraftPlan => f.write_str("existing_draft_plan"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidateToAuthoringSignal {
    PendingChecklist,
    IncompleteWork,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidateToAuthoringNextAction {
    ResumeAuthoringForRemainingPlanWork,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlanAutoApprovedDetail {
    pub auto_approved_in_agent_mode: bool,
    pub source: AutoApprovalSource,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ValidatePassToAuthoringDetail {
    pub signal: String,
    pub plan_key: Option<String>,
    pub pending_count: usize,
    pub pending_refs: Value,
    pub dbt_validate_observation: Value,
    pub next_action: ValidateToAuthoringNextAction,
    pub audit_acceptance: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlanInvalidEmptyDetail {
    pub plan_key: String,
    pub status: crate::plan_types::PlanStatus,
    pub tasks_len: usize,
    pub batches_len: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CleanseDraftUngroundedDetail {
    pub plan_key: String,
    pub reason: String,
    pub removed_non_raw: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlanPrunedEmptyDetail {
    pub plan_key: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlanSemanticInvalidErrorsDetail {
    pub plan_key: String,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReviewDecisionTransitionDetail {
    pub review_phase: String,
    pub meta: ReviewDecisionMeta,
    pub answer: ReviewDecision,
    pub forced_progress_guard: bool,
    pub forced_progress_by_subjective_retry: bool,
    pub review_subjective_retry_count: usize,
    pub trigger_step_idx: usize,
    pub trigger_step: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuthoringCompleteInvariantsDetail {
    pub has_dbt_project_yml: bool,
    pub has_any_models: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuthoringCompleteGuardStateDetail {
    pub last_validate_failed: bool,
    pub mutated_since_fail: bool,
    pub probe_required: bool,
    pub probe_satisfied: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuthoringCompleteReasonDetail {
    pub invariants: AuthoringCompleteInvariantsDetail,
    pub guard_state: AuthoringCompleteGuardStateDetail,
}

pub fn to_value<T: Serialize>(detail: &T) -> Value {
    react_core::workflow::reason_detail_value(detail)
}

pub fn review_decision_transition(
    review_phase: impl Into<String>,
    meta: ReviewDecisionMeta,
    answer: ReviewDecision,
    forced_progress_guard: bool,
    forced_progress_by_subjective_retry: bool,
    review_subjective_retry_count: usize,
    trigger_step_idx: usize,
    trigger_step: Value,
) -> Value {
    to_value(&ReviewDecisionTransitionDetail {
        review_phase: review_phase.into(),
        meta,
        answer,
        forced_progress_guard,
        forced_progress_by_subjective_retry,
        review_subjective_retry_count,
        trigger_step_idx,
        trigger_step,
    })
}
