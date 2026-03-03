use react_core::control_flow::GuardBlockKind;

use crate::data_engineer::control_flow::Phase;
use crate::data_engineer::progress_controller::{
    ExecutionMode, ExecutionState, FailedModelRef, PendingLoopbackIntent, RepairLadderStep,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreTurnDirective {
    Proceed,
    FailFast {
        kind: GuardBlockKind,
        reason: String,
    },
}

pub fn evaluate_pre_turn_directive(
    execution_state: &ExecutionState,
    phase: Phase,
    max_replan_backtracks: usize,
) -> PreTurnDirective {
    if execution_state.stall_count >= execution_state.max_stall_count
        && matches!(execution_state.mode, ExecutionMode::Mutate)
    {
        return PreTurnDirective::FailFast {
            kind: GuardBlockKind::AuthoringToValidate,
            reason: format!(
                "failed to make progress for this thread: execution_state stall_count={} reached max_stall_count={} in mode=mutate",
                execution_state.stall_count, execution_state.max_stall_count
            ),
        };
    }

    if execution_state.replan_backtracks >= max_replan_backtracks {
        return PreTurnDirective::FailFast {
            kind: GuardBlockKind::BatchLocked,
            reason: format!(
                "failed to make progress for this thread: observed {} validate/review loopback(s) to plan/author in active track '{}' since the last successful dbt_validate (limit {}). Stopping this thread. Please inspect the latest validate/review errors and apply a targeted fix before rerunning.",
                execution_state.replan_backtracks,
                phase.as_str(),
                max_replan_backtracks
            ),
        };
    }

    let fallback_failed_models = execution_state
        .last_validate
        .as_ref()
        .map(|v| v.failed_models.as_slice())
        .unwrap_or(&[]);
    let single_target_repair_path =
        derive_single_target_repair_path(execution_state, fallback_failed_models);
    if execution_state.hard_mutation_repair_mode
        && single_target_repair_path.is_some()
        && execution_state.ladder_step == RepairLadderStep::Stop
    {
        let target = single_target_repair_path
            .as_deref()
            .unwrap_or("(unknown target)")
            .trim()
            .to_string();
        return PreTurnDirective::FailFast {
            kind: GuardBlockKind::AuthoringToValidate,
            reason: format!(
                "failed to make progress for this thread: deterministic repair ladder reached stop for '{}' after {} attempt(s). Apply a manual fix to the target file and rerun the thread.",
                target,
                execution_state.attempt_count
            ),
        };
    }

    PreTurnDirective::Proceed
}

pub fn patch_plan_intent_blocks_fast_forward(
    execution_state: &ExecutionState,
    phase: Phase,
    current_plan_key: &str,
    current_plan_digest: Option<&str>,
) -> bool {
    let Some(intent) = execution_state.pending_loopback_intent.as_ref() else {
        return false;
    };
    match intent {
        PendingLoopbackIntent::PatchPlan {
            phase: intent_phase,
            entry_plan_key,
            entry_plan_digest,
        } => {
            if *intent_phase != phase {
                return false;
            }
            let key_changed = entry_plan_key
                .as_ref()
                .map(|k| k.trim() != current_plan_key.trim())
                .unwrap_or(true);
            let digest_changed = match (entry_plan_digest.as_deref(), current_plan_digest) {
                (Some(prev), Some(cur)) => prev.trim() != cur.trim(),
                (None, Some(_)) => true,
                _ => false,
            };
            !(key_changed || digest_changed)
        }
        _ => false,
    }
}

pub fn patch_impl_intent_unsatisfied(execution_state: &ExecutionState, phase: Phase) -> bool {
    let Some(intent) = execution_state.pending_loopback_intent.as_ref() else {
        return false;
    };
    match intent {
        PendingLoopbackIntent::PatchImpl {
            phase: intent_phase,
            entry_mutation_epoch,
        } => *intent_phase == phase && execution_state.mutation_epoch <= *entry_mutation_epoch,
        _ => false,
    }
}

pub fn derive_single_target_repair_path(
    execution_state: &ExecutionState,
    last_validate_failed_models: &[FailedModelRef],
) -> Option<String> {
    execution_state
        .single_target_repair_path
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            last_validate_failed_models
                .iter()
                .map(|fm| fm.file.trim().to_string())
                .find(|s| !s.is_empty() && s != "(unknown file)")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preturn_gate_prioritizes_mutate_stall_failfast() {
        let mut st = ExecutionState::new();
        st.mode = ExecutionMode::Mutate;
        st.stall_count = 3;
        st.max_stall_count = 3;
        let d = evaluate_pre_turn_directive(&st, Phase::CleanseAuthor, 3);
        match d {
            PreTurnDirective::FailFast { kind, .. } => {
                assert_eq!(kind, GuardBlockKind::AuthoringToValidate)
            }
            _ => panic!("expected failfast"),
        }
    }

    #[test]
    fn preturn_gate_replan_backtrack_failfast() {
        let mut st = ExecutionState::new();
        st.replan_backtracks = 4;
        let d = evaluate_pre_turn_directive(&st, Phase::ModelPlan, 3);
        match d {
            PreTurnDirective::FailFast { kind, reason } => {
                assert_eq!(kind, GuardBlockKind::BatchLocked);
                assert!(reason.contains("loopback"));
            }
            _ => panic!("expected failfast"),
        }
    }

    #[test]
    fn preturn_gate_hard_repair_ladder_stop_failfast() {
        let mut st = ExecutionState::new();
        st.hard_mutation_repair_mode = true;
        st.single_target_repair_path = Some("models/staging/stg_orders.sql".to_string());
        st.ladder_step = RepairLadderStep::Stop;
        st.attempt_count = 7;
        let d = evaluate_pre_turn_directive(&st, Phase::CleanseAuthor, 3);
        match d {
            PreTurnDirective::FailFast { kind, reason } => {
                assert_eq!(kind, GuardBlockKind::AuthoringToValidate);
                assert!(reason.contains("deterministic repair ladder reached stop"));
                assert!(reason.contains("stg_orders.sql"));
            }
            _ => panic!("expected failfast"),
        }
    }

    #[test]
    fn preturn_gate_hard_repair_ladder_stop_uses_failed_model_fallback() {
        let mut st = ExecutionState::new();
        st.hard_mutation_repair_mode = true;
        st.single_target_repair_path = None;
        st.ladder_step = RepairLadderStep::Stop;
        st.attempt_count = 2;
        st.last_validate = Some(crate::data_engineer::progress_controller::LastValidateState {
            failed_models: vec![crate::data_engineer::progress_controller::FailedModelRef {
                name: "stg_orders".to_string(),
                file: "models/staging/stg_orders.sql".to_string(),
            }],
            ..Default::default()
        });
        let d = evaluate_pre_turn_directive(&st, Phase::CleanseAuthor, 3);
        match d {
            PreTurnDirective::FailFast { kind, reason } => {
                assert_eq!(kind, GuardBlockKind::AuthoringToValidate);
                assert!(reason.contains("stg_orders.sql"));
            }
            _ => panic!("expected failfast"),
        }
    }

    #[test]
    fn preturn_gate_table_driven_precedence() {
        struct Case {
            name: &'static str,
            setup: fn(&mut ExecutionState),
            expect_fail: bool,
            expect_kind: Option<GuardBlockKind>,
        }
        let cases = vec![
            Case {
                name: "default_proceed",
                setup: |_| {},
                expect_fail: false,
                expect_kind: None,
            },
            Case {
                name: "stall_has_priority_over_replan",
                setup: |st| {
                    st.mode = ExecutionMode::Mutate;
                    st.stall_count = 3;
                    st.max_stall_count = 3;
                    st.replan_backtracks = 10;
                },
                expect_fail: true,
                expect_kind: Some(GuardBlockKind::AuthoringToValidate),
            },
            Case {
                name: "replan_failfast_when_no_stall",
                setup: |st| {
                    st.replan_backtracks = 5;
                },
                expect_fail: true,
                expect_kind: Some(GuardBlockKind::BatchLocked),
            },
        ];

        for c in cases {
            let mut st = ExecutionState::new();
            (c.setup)(&mut st);
            let d = evaluate_pre_turn_directive(&st, Phase::ModelPlan, 3);
            match (c.expect_fail, d) {
                (false, PreTurnDirective::Proceed) => {}
                (true, PreTurnDirective::FailFast { kind, .. }) => {
                    assert_eq!(Some(kind), c.expect_kind, "case={}", c.name);
                }
                _ => panic!("unexpected directive for case={}", c.name),
            }
        }
    }
}
