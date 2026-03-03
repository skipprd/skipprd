use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Debug, Serialize)]
pub struct PlanActionableAutoApprovedDetail {
    pub plan_key: String,
    pub entry_reason_code: String,
    pub plan_update_summary: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_step_idx: Option<usize>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PlanAutoApprovedDetail {
    pub auto_approved_in_agent_mode: bool,
    pub source: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ValidatePassToAuthoringDetail {
    pub signal: String,
    pub plan_key: Option<String>,
    pub pending_count: usize,
    pub pending_refs: Value,
    pub dbt_validate_observation: Value,
    pub next_action: String,
    pub audit_acceptance: Value,
}

#[derive(Clone, Debug, Serialize)]
pub struct ValidatePassToReviewDetail {
    pub dbt_validate_observation: Value,
    pub dbt_validate_step_idx: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct ValidateFailDetail {
    pub dbt_validate_observation: Value,
    pub dbt_validate_step_idx: usize,
    pub errors: Vec<String>,
    pub facts_bundle: Value,
}

#[derive(Clone, Debug, Serialize)]
pub struct PlanMissingDetail {
    pub plan_kind: String,
    pub note: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct PlanNotApprovedDetail {
    pub status: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct PlanKeyDetail {
    pub plan_key: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct PlanSemanticInvalidDetail {
    pub plan_key: String,
    pub reason: String,
    pub audit_acceptance: Value,
}

#[derive(Clone, Debug, Serialize)]
pub struct PlanInvalidEmptyDetail {
    pub plan_key: String,
    pub status: String,
    pub tasks_len: usize,
    pub batches_len: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct CleanseDraftUngroundedDetail {
    pub plan_key: String,
    pub reason: String,
    pub removed_non_raw: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct PlanPrunedEmptyDetail {
    pub plan_key: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct PlanSemanticInvalidErrorsDetail {
    pub plan_key: String,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReviewDecisionTransitionDetail {
    pub review_phase: String,
    pub meta: Value,
    pub answer: String,
    pub forced_progress_guard: bool,
    pub forced_progress_by_subjective_retry: bool,
    pub review_subjective_retry_count: usize,
    pub trigger_step_idx: usize,
    pub trigger_step: Value,
}

#[derive(Clone, Debug, Serialize)]
pub struct PublishApprovalStateDetail {
    pub approval_state: Value,
}

#[derive(Clone, Debug, Serialize)]
pub struct PublishObservationDetail {
    pub publish_observation: Value,
}

#[derive(Clone, Debug, Serialize)]
pub struct PublishAutoApprovedDetail {
    pub auto_approved_in_agent_mode: bool,
    pub publish_observation: Value,
}

#[derive(Clone, Debug, Serialize)]
pub struct PublishFailureDetail {
    pub publish_observation: Value,
    pub publish_failure_retry_count: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct AuthoringCompleteInvariantsDetail {
    pub has_dbt_project_yml: bool,
    pub has_any_models: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct AuthoringCompleteGuardStateDetail {
    pub last_validate_failed: bool,
    pub mutated_since_fail: bool,
    pub patched_since_fail: bool,
    pub mutation_failures_since_validate: usize,
    pub probe_required: bool,
    pub probe_satisfied: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct AuthoringCompleteReasonDetail {
    pub invariants: AuthoringCompleteInvariantsDetail,
    pub guard_state: AuthoringCompleteGuardStateDetail,
}

pub fn to_value<T: Serialize>(detail: &T) -> Value {
    serde_json::to_value(detail).unwrap_or(Value::Null)
}

pub fn plan_actionable_auto_approved(
    plan_key: impl Into<String>,
    plan_update_summary: Value,
    entry_step_idx: Option<usize>,
) -> Value {
    to_value(&PlanActionableAutoApprovedDetail {
        plan_key: plan_key.into(),
        entry_reason_code: "review_actionable_true".to_string(),
        plan_update_summary,
        entry_step_idx,
    })
}

pub fn plan_auto_approved(source: impl Into<String>) -> Value {
    to_value(&PlanAutoApprovedDetail {
        auto_approved_in_agent_mode: true,
        source: source.into(),
    })
}

pub fn review_decision_transition(
    review_phase: impl Into<String>,
    meta: Value,
    answer: impl Into<String>,
    forced_progress_guard: bool,
    forced_progress_by_subjective_retry: bool,
    review_subjective_retry_count: usize,
    trigger_step_idx: usize,
    trigger_step: Value,
) -> Value {
    to_value(&ReviewDecisionTransitionDetail {
        review_phase: review_phase.into(),
        meta,
        answer: answer.into(),
        forced_progress_guard,
        forced_progress_by_subjective_retry,
        review_subjective_retry_count,
        trigger_step_idx,
        trigger_step,
    })
}

pub fn publish_approval_state(approval_state: Value) -> Value {
    to_value(&PublishApprovalStateDetail { approval_state })
}

pub fn publish_observation(publish_observation: Value) -> Value {
    to_value(&PublishObservationDetail {
        publish_observation,
    })
}

pub fn publish_auto_approved(publish_observation: Value) -> Value {
    to_value(&PublishAutoApprovedDetail {
        auto_approved_in_agent_mode: true,
        publish_observation,
    })
}

pub fn publish_failure(
    publish_observation: Value,
    publish_failure_retry_count: usize,
) -> Value {
    to_value(&PublishFailureDetail {
        publish_observation,
        publish_failure_retry_count,
    })
}

pub fn plan_missing(plan_kind: impl Into<String>, note: impl Into<String>) -> Value {
    to_value(&PlanMissingDetail {
        plan_kind: plan_kind.into(),
        note: note.into(),
    })
}

pub fn plan_not_approved(status: impl Into<String>) -> Value {
    to_value(&PlanNotApprovedDetail {
        status: status.into(),
    })
}

pub fn plan_semantic_invalid(
    plan_key: impl Into<String>,
    reason: impl Into<String>,
    audit_acceptance: Value,
) -> Value {
    to_value(&PlanSemanticInvalidDetail {
        plan_key: plan_key.into(),
        reason: reason.into(),
        audit_acceptance,
    })
}

pub fn plan_key(plan_key: impl Into<String>) -> Value {
    to_value(&PlanKeyDetail {
        plan_key: plan_key.into(),
    })
}

