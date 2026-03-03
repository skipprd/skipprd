use serde::Serialize;
use serde_json::Value;

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

pub fn to_value<T: Serialize>(detail: &T) -> Value {
    serde_json::to_value(detail).unwrap_or(Value::Null)
}

