use serde::{Deserialize, Serialize};
use serde_json::Value;

use react_core::session::ThreadStore;

pub const REPAIR_STATE_SCHEMA_VERSION: u32 = 1;
pub const REPAIR_STATE_ARTIFACT_ID: &str = "repair_state";

/// Deterministic repair ladder step.
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

/// Suite-owned, bounded repair state for post-validate failure convergence.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RepairState {
    pub schema_version: u32,
    #[serde(default)]
    pub entered_at_step_idx: Option<usize>,
    #[serde(default)]
    pub target_path: Option<String>,
    #[serde(default)]
    pub attempt_count: usize,
    #[serde(default)]
    pub consecutive_noop_patches: usize,
    #[serde(default)]
    pub last_error_class: Option<String>,
    #[serde(default)]
    pub last_error_brief: Option<String>,
    /// Latest failing-model evidence (bounded).
    #[serde(default)]
    pub last_failed_models: Vec<Value>,
    #[serde(default)]
    pub ladder_step: RepairLadderStep,
}

impl RepairState {
    pub fn new() -> Self {
        Self {
            schema_version: REPAIR_STATE_SCHEMA_VERSION,
            ..Default::default()
        }
    }

    pub async fn load(thread_store: &ThreadStore, thread_id: &str) -> Option<Self> {
        let v = thread_store
            .get_thread_artifact_json(thread_id, REPAIR_STATE_ARTIFACT_ID)
            .await
            .ok()?;
        let st = serde_json::from_value::<Self>(v).ok()?;
        if st.schema_version != REPAIR_STATE_SCHEMA_VERSION {
            return None;
        }
        Some(st)
    }

    pub async fn save(&self, thread_store: &ThreadStore, thread_id: &str) -> Result<(), String> {
        if self.schema_version != REPAIR_STATE_SCHEMA_VERSION {
            return Err(format!(
                "repair_state schema_version mismatch: expected {}, got {}",
                REPAIR_STATE_SCHEMA_VERSION, self.schema_version
            ));
        }
        let v = serde_json::to_value(self).map_err(|e| e.to_string())?;
        thread_store
            .put_thread_artifact_json(thread_id, REPAIR_STATE_ARTIFACT_ID, &v)
            .await
    }
}

