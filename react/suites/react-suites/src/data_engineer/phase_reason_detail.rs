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

pub fn to_value<T: Serialize>(detail: &T) -> Value {
    serde_json::to_value(detail).unwrap_or(Value::Null)
}

