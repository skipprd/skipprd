use serde::{Deserialize, Serialize};
use serde_json::Value;

use react_core::session::ThreadStore;

pub const CONTROL_STATE_SCHEMA_VERSION: u32 = 1;
pub const CONTROL_STATE_ARTIFACT_ID: &str = "control_state";

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LastValidateState {
    /// Step index in the thread log when this validate completed (best-effort).
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
    /// Suite-level brief summary (bounded, intended for prompts).
    #[serde(default)]
    pub brief: Option<String>,
    /// Parsed failing models (name/file), already extracted from dbt output.
    #[serde(default)]
    pub failed_models: Vec<Value>,
    /// Coarse classification: sql_or_runtime | schema_or_precheck | unknown.
    #[serde(default)]
    pub failure_class: Option<String>,
}

/// Suite-owned, compact control state for deterministic decisions.
///
/// This is intentionally bounded and stable. It should be updated incrementally,
/// and treated as the primary runtime decision source (thread log remains audit-only).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ControlState {
    pub schema_version: u32,
    #[serde(default)]
    pub current_phase: Option<String>,
    #[serde(default)]
    pub phase_reason_code: Option<String>,
    #[serde(default)]
    pub last_validate: Option<LastValidateState>,
    #[serde(default)]
    pub replan_backtracks: usize,
    #[serde(default)]
    pub hard_mutation_repair_mode: bool,
    #[serde(default)]
    pub single_target_repair_path: Option<String>,
}

impl ControlState {
    pub fn new() -> Self {
        Self {
            schema_version: CONTROL_STATE_SCHEMA_VERSION,
            ..Default::default()
        }
    }

    pub async fn load(thread_store: &ThreadStore, thread_id: &str) -> Option<Self> {
        let v = thread_store
            .get_thread_artifact_json(thread_id, CONTROL_STATE_ARTIFACT_ID)
            .await
            .ok()?;
        let st = serde_json::from_value::<Self>(v).ok()?;
        if st.schema_version != CONTROL_STATE_SCHEMA_VERSION {
            return None;
        }
        Some(st)
    }

    pub async fn save(&self, thread_store: &ThreadStore, thread_id: &str) -> Result<(), String> {
        if self.schema_version != CONTROL_STATE_SCHEMA_VERSION {
            return Err(format!(
                "control_state schema_version mismatch: expected {}, got {}",
                CONTROL_STATE_SCHEMA_VERSION, self.schema_version
            ));
        }
        let v = serde_json::to_value(self).map_err(|e| e.to_string())?;
        thread_store
            .put_thread_artifact_json(thread_id, CONTROL_STATE_ARTIFACT_ID, &v)
            .await
    }
}
