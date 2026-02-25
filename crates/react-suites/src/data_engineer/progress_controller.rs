use serde::{Deserialize, Serialize};
use serde_json::Value;

use react_core::session::{ThreadLog, ThreadStore};

use crate::data_engineer::control_flow::{self, Phase};

pub const EXECUTION_STATE_SCHEMA_VERSION: u32 = 1;
pub const EXECUTION_STATE_ARTIFACT_ID: &str = "execution_state";

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
    pub failed_models: Vec<Value>,
    #[serde(default)]
    pub failure_class: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionTier {
    Cleanse,
    Model,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    Mutate,
    Validate,
    Done,
    Failed,
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

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RepairTarget {
    #[serde(default)]
    pub model_name: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub error_class: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProgressDelta {
    #[serde(default)]
    pub target_hash_changed: bool,
    #[serde(default)]
    pub failed_target_count_delta: i64,
    #[serde(default)]
    pub validate_error_fingerprint_delta: bool,
    #[serde(default)]
    pub checklist_completed_delta: i64,
    #[serde(default)]
    pub progress_made: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionState {
    pub schema_version: u32,
    #[serde(default)]
    pub current_phase: Option<String>,
    #[serde(default)]
    pub phase_reason_code: Option<String>,
    #[serde(default)]
    pub replan_backtracks: usize,
    #[serde(default)]
    pub last_validate: Option<LastValidateState>,
    #[serde(default)]
    pub current_tier: Option<ExecutionTier>,
    #[serde(default)]
    pub mode: Option<ExecutionMode>,
    #[serde(default)]
    pub last_validate_ok: Option<bool>,
    #[serde(default)]
    pub last_validate_error_fingerprint: Option<String>,
    #[serde(default)]
    pub repair_backlog: Vec<RepairTarget>,
    #[serde(default)]
    pub hard_mutation_repair_mode: bool,
    #[serde(default)]
    pub single_target_repair_path: Option<String>,
    #[serde(default)]
    pub target_path: Option<String>,
    #[serde(default)]
    pub ladder_step: RepairLadderStep,
    #[serde(default)]
    pub attempt_count: usize,
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
    pub last_error_class: Option<String>,
    #[serde(default)]
    pub last_failed_models: Vec<Value>,
}

impl ExecutionState {
    pub fn new() -> Self {
        Self {
            schema_version: EXECUTION_STATE_SCHEMA_VERSION,
            max_stall_count: 3,
            ..Default::default()
        }
    }

    pub async fn load(thread_store: &ThreadStore, thread_id: &str) -> Option<Self> {
        let v = thread_store
            .get_thread_artifact_json(thread_id, EXECUTION_STATE_ARTIFACT_ID)
            .await
            .ok()?;
        let st = serde_json::from_value::<Self>(v).ok()?;
        if st.schema_version != EXECUTION_STATE_SCHEMA_VERSION {
            return None;
        }
        Some(st)
    }

    pub async fn save(&self, thread_store: &ThreadStore, thread_id: &str) -> Result<(), String> {
        if self.schema_version != EXECUTION_STATE_SCHEMA_VERSION {
            return Err(format!(
                "execution_state schema_version mismatch: expected {}, got {}",
                EXECUTION_STATE_SCHEMA_VERSION, self.schema_version
            ));
        }
        let v = serde_json::to_value(self).map_err(|e| e.to_string())?;
        thread_store
            .put_thread_artifact_json(thread_id, EXECUTION_STATE_ARTIFACT_ID, &v)
            .await
    }

    pub fn apply_validate_success(&mut self, tier: ExecutionTier) {
        self.current_tier = Some(tier);
        self.mode = Some(ExecutionMode::Done);
        self.last_validate = Some(LastValidateState {
            ts: Some(chrono::Utc::now().to_rfc3339()),
            ok: Some(true),
            compile_ok: Some(true),
            run_ok: Some(true),
            ..LastValidateState::default()
        });
        self.last_validate_ok = Some(true);
        self.last_validate_error_fingerprint = None;
        self.repair_backlog.clear();
        self.hard_mutation_repair_mode = false;
        self.single_target_repair_path = None;
        self.target_path = None;
        self.ladder_step = RepairLadderStep::PatchTarget;
        self.attempt_count = 0;
        self.consecutive_noop_patches = 0;
        self.stall_count = 0;
        self.last_progress_delta = Some(ProgressDelta {
            progress_made: true,
            ..ProgressDelta::default()
        });
        self.last_error_class = None;
        self.last_failed_models.clear();
        self.last_error_brief = None;
    }

    pub fn apply_validate_failure(
        &mut self,
        tier: ExecutionTier,
        error_fingerprint: String,
        backlog: Vec<RepairTarget>,
        brief: Option<String>,
    ) {
        let prev_count = self.repair_backlog.len() as i64;
        let prev_fp = self.last_validate_error_fingerprint.clone();
        self.current_tier = Some(tier);
        self.mode = Some(ExecutionMode::Mutate);
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
                .map(|t| {
                    serde_json::json!({
                        "name": t.model_name.clone().unwrap_or_default(),
                        "file": t.path.clone().unwrap_or_default(),
                    })
                })
                .collect(),
            failure_class: None,
            ..LastValidateState::default()
        });
        self.last_validate_ok = Some(false);
        self.last_validate_error_fingerprint = Some(error_fingerprint.clone());
        self.repair_backlog = backlog;
        self.hard_mutation_repair_mode = true;
        self.single_target_repair_path = self
            .repair_backlog
            .iter()
            .find_map(|t| t.path.as_ref().map(|s| s.trim().to_string()))
            .filter(|s| !s.is_empty());
        self.target_path = self.single_target_repair_path.clone();
        self.ladder_step = RepairLadderStep::PatchTarget;
        self.attempt_count = 0;
        self.consecutive_noop_patches = 0;
        self.last_error_brief = brief;
        self.last_error_class = Some(error_fingerprint.clone());
        self.last_failed_models = self
            .repair_backlog
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.model_name.clone().unwrap_or_default(),
                    "file": t.path.clone().unwrap_or_default(),
                })
            })
            .collect();

        let failed_target_count_delta = self.repair_backlog.len() as i64 - prev_count;
        let validate_error_fingerprint_delta =
            prev_fp.as_deref().unwrap_or("") != error_fingerprint.as_str();
        let progress_made = failed_target_count_delta != 0 || validate_error_fingerprint_delta;
        if progress_made {
            self.stall_count = 0;
        } else {
            self.stall_count = self.stall_count.saturating_add(1);
        }
        self.last_progress_delta = Some(ProgressDelta {
            target_hash_changed: false,
            failed_target_count_delta,
            validate_error_fingerprint_delta,
            checklist_completed_delta: 0,
            progress_made,
        });
    }

    pub fn note_patch_attempt(&mut self, ok: bool, mutated: bool) {
        self.attempt_count = self.attempt_count.saturating_add(1);
        if ok && mutated {
            self.consecutive_noop_patches = 0;
            self.ladder_step = RepairLadderStep::PatchTarget;
            self.stall_count = 0;
            self.last_progress_delta = Some(ProgressDelta {
                target_hash_changed: true,
                progress_made: true,
                ..ProgressDelta::default()
            });
            return;
        }
        self.consecutive_noop_patches = self.consecutive_noop_patches.saturating_add(1);
        self.ladder_step = if self.attempt_count >= 2 {
            RepairLadderStep::Stop
        } else {
            RepairLadderStep::ReplaceFile
        };
        self.stall_count = self.stall_count.saturating_add(1);
        self.last_progress_delta = Some(ProgressDelta {
            target_hash_changed: false,
            progress_made: false,
            ..ProgressDelta::default()
        });
    }
}

fn obs_like_bool(
    from: &Option<LastValidateState>,
    pick: impl FnOnce(&LastValidateState) -> Option<bool>,
) -> Option<bool> {
    from.as_ref().and_then(pick)
}

pub fn error_fingerprint_from_validate_obs(obs: &Value) -> String {
    let errs = obs
        .get("errors")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .take(8)
                .collect::<Vec<_>>()
                .join(" | ")
        })
        .unwrap_or_default();
    let class = if obs.get("compile_ok").and_then(|v| v.as_bool()).unwrap_or(false)
        && !obs.get("run_ok").and_then(|v| v.as_bool()).unwrap_or(false)
    {
        "runtime"
    } else if !obs.get("compile_ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        "compile"
    } else {
        "unknown"
    };
    format!("{}::{}", class, errs)
}

pub fn repair_backlog_from_failed_models(failing_models: &[Value]) -> Vec<RepairTarget> {
    let mut out: Vec<RepairTarget> = failing_models
        .iter()
        .map(|fm| RepairTarget {
            model_name: fm
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            path: fm
                .get("file")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty() && s != "(unknown file)"),
            error_class: None,
        })
        .collect();
    out.sort_by(|a, b| a.path.cmp(&b.path).then(a.model_name.cmp(&b.model_name)));
    out.dedup_by(|a, b| a.path == b.path && a.model_name == b.model_name);
    out
}

pub fn gate_authoring_progress(log: Option<&ThreadLog>, phase: Phase) -> Result<(), String> {
    let g = control_flow::derive_guard_state(log);
    if g.last_validate_failed && !(g.mutated_since_fail || g.patched_since_fail) {
        return Err(
            "progress_gate_blocked: validation previously failed and no successful mutation has been recorded since that failure"
                .to_string(),
        );
    }
    if g.probe_required && !g.probe_satisfied {
        return Err(
            "progress_gate_blocked: runtime validation previously failed after compile and a data probe is still required"
                .to_string(),
        );
    }
    // Keep existing unresolved mutation-failure behavior, but as a deterministic progress gate.
    match phase {
        Phase::CleanseAuthor | Phase::ModelAuthor => {}
        _ => return Ok(()),
    }
    Ok(())
}
