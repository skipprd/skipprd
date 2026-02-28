use serde::{Deserialize, Serialize};
use serde_json::Value;

use react_core::control_flow::PhaseReasonCode;
use react_core::session::ThreadStore;

use crate::data_engineer::control_flow::Phase;

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
    pub failure_class: Option<FailureClass>,
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

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionState {
    pub schema_version: u32,
    #[serde(default)]
    pub current_phase: Option<Phase>,
    #[serde(default)]
    pub phase_reason_code: Option<PhaseReasonCode>,
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
    pub last_error_class: Option<FailureClass>,
    #[serde(default)]
    pub last_failed_models: Vec<Value>,
    #[serde(default)]
    pub subjective_retry_phase: String,
    #[serde(default)]
    pub subjective_retry_kind: String,
    #[serde(default)]
    pub subjective_retry_count: usize,
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
        react_core::state::load_thread_state_artifact::<Self>(thread_store, thread_id).await
    }

    pub async fn save(&self, thread_store: &ThreadStore, thread_id: &str) -> Result<(), String> {
        if self.schema_version != EXECUTION_STATE_SCHEMA_VERSION {
            return Err(format!(
                "execution_state schema_version mismatch: expected {}, got {}",
                EXECUTION_STATE_SCHEMA_VERSION, self.schema_version
            ));
        }
        react_core::state::save_thread_state_artifact(thread_store, thread_id, self).await
    }

    pub fn apply_validate_success(&mut self, tier: ExecutionTier) {
        self.current_tier = tier;
        self.mode = ExecutionMode::Done;
        self.last_validate = Some(LastValidateState {
            ts: Some(chrono::Utc::now().to_rfc3339()),
            ok: Some(true),
            compile_ok: Some(true),
            run_ok: Some(true),
            ..LastValidateState::default()
        });
        self.last_validate_ok = Some(true);
        self.last_failure_signature = None;
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
        self.subjective_retry_count = 0;
        self.subjective_retry_kind.clear();
        self.subjective_retry_phase.clear();
    }

    pub fn apply_validate_failure(
        &mut self,
        tier: ExecutionTier,
        failure_class: FailureClass,
        failure_signature: FailureSignature,
        backlog: Vec<RepairTarget>,
        brief: Option<String>,
    ) {
        let prev_count = self.repair_backlog.len() as i64;
        let prev_signature = self.last_failure_signature.clone();
        self.current_tier = tier;
        self.mode = ExecutionMode::Mutate;
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
            failure_class: Some(failure_class),
            ..LastValidateState::default()
        });
        self.last_validate_ok = Some(false);
        self.last_failure_signature = Some(failure_signature.clone());
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
        self.last_error_class = Some(failure_class);
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
        let failure_signature_changed = prev_signature != Some(failure_signature);
        let progress_made = failed_target_count_delta < 0;
        if progress_made {
            self.stall_count = 0;
        } else {
            self.stall_count = self.stall_count.saturating_add(1);
        }
        self.last_progress_delta = Some(ProgressDelta {
            target_hash_changed: false,
            failed_target_count_delta,
            failure_signature_changed,
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

    pub fn bump_subjective_retry(&mut self, phase: Phase, kind: &str, cap: usize) -> usize {
        let capped = cap.max(1);
        let phase_name = phase.as_str().to_string();
        let kind_name = kind.to_string();
        if self.subjective_retry_phase == phase_name && self.subjective_retry_kind == kind_name {
            self.subjective_retry_count = self.subjective_retry_count.saturating_add(1).min(capped);
        } else {
            self.subjective_retry_phase = phase_name;
            self.subjective_retry_kind = kind_name;
            self.subjective_retry_count = 1.min(capped);
        }
        self.subjective_retry_count
    }

    pub fn reset_subjective_retry(&mut self) {
        self.subjective_retry_count = 0;
        self.subjective_retry_phase.clear();
        self.subjective_retry_kind.clear();
    }

    pub fn enter_validate_mode(&mut self, tier: ExecutionTier) {
        self.current_tier = tier;
        self.mode = ExecutionMode::Validate;
    }

    pub fn mark_failed(&mut self, brief: impl Into<String>) {
        self.mode = ExecutionMode::Failed;
        self.last_error_brief = Some(brief.into());
    }
}

impl react_core::state::ThreadStateArtifact for ExecutionState {
    const ARTIFACT_ID: &'static str = EXECUTION_STATE_ARTIFACT_ID;
    const SCHEMA_VERSION: u32 = EXECUTION_STATE_SCHEMA_VERSION;

    fn schema_version(&self) -> u32 {
        self.schema_version
    }
}

fn obs_like_bool(
    from: &Option<LastValidateState>,
    pick: impl FnOnce(&LastValidateState) -> Option<bool>,
) -> Option<bool> {
    from.as_ref().and_then(pick)
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
    let compile_ok = state
        .last_validate
        .as_ref()
        .and_then(|v| v.compile_ok)
        .unwrap_or(false);
    let run_ok = state
        .last_validate
        .as_ref()
        .and_then(|v| v.run_ok)
        .unwrap_or(false);
    let probe_required = last_validate_failed && compile_ok;
    let probe_satisfied = !probe_required || run_ok;
    if probe_required && !probe_satisfied {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_has_compile_time_defaults() {
        let st = ExecutionState::new();
        assert_eq!(st.current_tier, ExecutionTier::Unknown);
        assert_eq!(st.mode, ExecutionMode::Discover);
    }

    #[test]
    fn validate_success_resets_repair_and_retry_state() {
        let mut st = ExecutionState::new();
        st.hard_mutation_repair_mode = true;
        st.subjective_retry_phase = "model_plan".to_string();
        st.subjective_retry_kind = "plan_json_invalid".to_string();
        st.subjective_retry_count = 3;
        st.apply_validate_success(ExecutionTier::Model);
        assert_eq!(st.current_tier, ExecutionTier::Model);
        assert_eq!(st.mode, ExecutionMode::Done);
        assert!(!st.hard_mutation_repair_mode);
        assert_eq!(st.subjective_retry_count, 0);
        assert!(st.subjective_retry_phase.is_empty());
    }

    #[test]
    fn subjective_retry_is_single_state_and_bounded() {
        let mut st = ExecutionState::new();
        assert_eq!(st.bump_subjective_retry(Phase::CleansePlan, "x", 3), 1);
        assert_eq!(st.bump_subjective_retry(Phase::CleansePlan, "x", 3), 2);
        assert_eq!(st.bump_subjective_retry(Phase::CleansePlan, "x", 3), 3);
        assert_eq!(st.bump_subjective_retry(Phase::CleansePlan, "x", 3), 3);
        assert_eq!(st.bump_subjective_retry(Phase::ModelPlan, "x", 3), 1);
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
        assert!(gate_authoring_progress(&st, Phase::ModelAuthor).is_ok());
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
}
