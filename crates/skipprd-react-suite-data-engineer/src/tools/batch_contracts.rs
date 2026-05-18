use serde::{Deserialize, Serialize};
use serde_json::Value;

#[allow(dead_code)]
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct CleanseSqlBatchContract {
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub reason_code: Option<Value>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub errors: Vec<String>,
    #[serde(default)]
    pub attempted_dataset_ids: Vec<String>,
    #[serde(default)]
    pub succeeded_dataset_ids: Vec<String>,
    #[serde(default)]
    pub failed_dataset_ids: Vec<String>,
    #[serde(default)]
    pub pending_schema_contract_dataset_ids: Vec<String>,
    #[serde(default)]
    pub blocked_dataset_ids: Vec<String>,
    #[serde(default)]
    pub done: Option<bool>,
    #[serde(default)]
    pub progress_made: Option<bool>,
    #[serde(default)]
    pub plan_key: Option<String>,
    #[serde(default)]
    pub auto_healed_wildcard_sql_dataset_ids: Vec<String>,
    #[serde(default)]
    pub inner: Option<Value>,
}

#[allow(dead_code)]
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelSqlBatchContract {
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub reason_code: Option<Value>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub errors: Vec<String>,
    #[serde(default)]
    pub attempted_item_names: Vec<String>,
    #[serde(default)]
    pub succeeded_item_names: Vec<String>,
    #[serde(default)]
    pub failed_item_names: Vec<String>,
    #[serde(default)]
    pub pending_schema_contract_item_names: Vec<String>,
    #[serde(default)]
    pub blocked_item_names: Vec<String>,
    #[serde(default)]
    pub done: Option<bool>,
    #[serde(default)]
    pub progress_made: Option<bool>,
    #[serde(default)]
    pub plan_key: Option<String>,
    #[serde(default)]
    pub inner: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CleanseSchemaBatchContract {
    #[serde(default)]
    pub ok: bool,
    pub checklist_item_id: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub reason_code: Option<Value>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub errors: Vec<String>,
    #[serde(default)]
    pub attempted_dataset_ids: Vec<String>,
    #[serde(default)]
    pub succeeded_dataset_ids: Vec<String>,
    #[serde(default)]
    pub failed_dataset_ids: Vec<String>,
    #[serde(default)]
    pub progress_made: Option<bool>,
    #[serde(default)]
    pub auto_healed_wildcard_sql_dataset_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plan_violations: Vec<PlanViolationBrief>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PlanViolationBrief {
    pub task_id: String,
    pub evidence: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelSchemaBatchContract {
    #[serde(default)]
    pub ok: bool,
    pub checklist_item_id: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub reason_code: Option<Value>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub errors: Vec<String>,
    #[serde(default)]
    pub attempted_item_names: Vec<String>,
    #[serde(default)]
    pub succeeded_item_names: Vec<String>,
    #[serde(default)]
    pub failed_item_names: Vec<String>,
    #[serde(default)]
    pub progress_made: Option<bool>,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plan_violations: Vec<PlanViolationBrief>,
}

pub(crate) fn to_json_value<T: Serialize>(v: T) -> Result<Value, String> {
    serde_json::to_value(v).map_err(|e| format!("failed to serialize batch contract: {e}"))
}
