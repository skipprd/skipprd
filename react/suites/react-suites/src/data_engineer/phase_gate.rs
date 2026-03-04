use react_core::control_flow::GuardBlockKind;

use crate::data_engineer::control_flow::Phase;
use crate::data_engineer::progress_controller::{
    ExecutionMode, ExecutionState, PendingLoopbackIntent, RepairLadderStep,
    DEFAULT_MAX_STALL_COUNT,
};

pub type PreTurnDirective = react_core::workflow::PreTurnDirective;

pub fn evaluate_pre_turn_directive(
    execution_state: &ExecutionState,
    phase: Phase,
    max_replan_backtracks: usize,
) -> PreTurnDirective {
    let phase_state = execution_state.phase_state();
    let repair_state = execution_state.repair_state();
    let core_snapshot = react_core::workflow::PreTurnStateSnapshot {
        mode_is_mutate: matches!(phase_state.mode, ExecutionMode::Mutate),
        stall_count: repair_state.stall_count,
        max_stall_count: DEFAULT_MAX_STALL_COUNT,
        replan_backtracks: phase_state.replan_backtracks,
        hard_mutation_repair_mode: false,
        ladder_stop: false,
        target_path: None,
        attempt_count: 0,
    };
    let core_eval = react_core::workflow::evaluate_pre_turn_directive(
        &core_snapshot,
        max_replan_backtracks,
    );
    if let PreTurnDirective::FailFast { kind, reason } = core_eval {
        if kind == GuardBlockKind::BatchLocked {
            return PreTurnDirective::FailFast {
                kind,
                reason: format!(
                    "failed to make progress for this thread: observed {} validate/review loopback(s) to plan/author in active track '{}' since the last successful dbt_validate (limit {}). Stopping this thread. Please inspect the latest validate/review errors and apply a targeted fix before rerunning.",
                    phase_state.replan_backtracks,
                    phase.as_str(),
                    max_replan_backtracks
                ),
            };
        }
        return PreTurnDirective::FailFast { kind, reason };
    }

    let single_target_repair_path = derive_single_target_repair_path(execution_state);
    let core_repair_snapshot = react_core::workflow::PreTurnStateSnapshot {
        mode_is_mutate: false,
        stall_count: 0,
        max_stall_count: 1,
        replan_backtracks: 0,
        hard_mutation_repair_mode: repair_state.hard_mutation_repair_mode()
            && single_target_repair_path.is_some(),
        ladder_stop: repair_state.ladder_step() == RepairLadderStep::Stop,
        target_path: single_target_repair_path.clone(),
        attempt_count: repair_state.attempt_count(),
    };
    react_core::workflow::evaluate_pre_turn_directive(&core_repair_snapshot, usize::MAX)
}

pub fn patch_plan_intent_blocks_fast_forward(
    execution_state: &ExecutionState,
    phase: Phase,
    current_plan_key: &str,
    current_plan_digest: Option<&str>,
) -> bool {
    let repair = execution_state.repair_state();
    let Some(intent) = repair.pending_loopback_intent.as_ref() else {
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
    let repair = execution_state.repair_state();
    let Some(intent) = repair.pending_loopback_intent.as_ref() else {
        return false;
    };
    match intent {
        PendingLoopbackIntent::PatchImpl {
            phase: intent_phase,
            entry_mutation_epoch,
        } => *intent_phase == phase && repair.mutation_epoch <= *entry_mutation_epoch,
        _ => false,
    }
}

pub fn derive_single_target_repair_path(execution_state: &ExecutionState) -> Option<String> {
    let repair = execution_state.repair_state();
    repair
        .single_target_repair_path()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preturn_gate_prioritizes_mutate_stall_failfast() {
        let mut st = ExecutionState::new();
        st.phase.mode = ExecutionMode::Mutate;
        st.repair.stall_count = 3;
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
        st.phase.replan_backtracks = 4;
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
        st.repair.repair_mode = crate::data_engineer::progress_controller::RepairModeState::SqlTarget(
            crate::data_engineer::progress_controller::SqlTargetRepairMode {
                target_path: crate::data_engineer::progress_controller::SqlModelPath::parse(
                    "models/staging/stg_orders.sql".to_string(),
                )
                .expect("valid sql model path"),
                ladder_step: RepairLadderStep::Stop,
                attempt_count: 7,
                repair_started_mutation_epoch: None,
                consecutive_noop_patches: 0,
            },
        );
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
        st.repair.repair_mode = crate::data_engineer::progress_controller::RepairModeState::SqlTarget(
            crate::data_engineer::progress_controller::SqlTargetRepairMode {
                target_path: crate::data_engineer::progress_controller::SqlModelPath::parse(
                    "models/staging/stg_orders.sql".to_string(),
                )
                .expect("valid sql model path"),
                ladder_step: RepairLadderStep::Stop,
                attempt_count: 3,
                repair_started_mutation_epoch: None,
                consecutive_noop_patches: 0,
            },
        );
        st.telemetry.last_validate = Some(crate::data_engineer::progress_controller::LastValidateState {
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
                    st.phase.mode = ExecutionMode::Mutate;
                    st.repair.stall_count = 3;
                    st.phase.replan_backtracks = 10;
                },
                expect_fail: true,
                expect_kind: Some(GuardBlockKind::AuthoringToValidate),
            },
            Case {
                name: "replan_failfast_when_no_stall",
                setup: |st| {
                    st.phase.replan_backtracks = 5;
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

    #[test]
    fn control_critical_plan_and_review_reason_details_use_typed_constructors() {
        let phase_plan_src = include_str!("phase_plan.rs");
        assert!(
            phase_plan_src.contains("phase_reason_detail::plan_actionable_auto_approved("),
            "phase_plan must use typed constructor for review-actionable auto-approval detail"
        );
        assert!(
            phase_plan_src.contains("phase_reason_detail::plan_auto_approved("),
            "phase_plan must use typed constructor for plan auto-approval detail"
        );
        assert!(
            !phase_plan_src.contains("PlanActionableAutoApprovedDetail {"),
            "phase_plan must not inline PlanActionableAutoApprovedDetail literals in transition paths"
        );
        assert!(
            !phase_plan_src.contains("PlanAutoApprovedDetail {"),
            "phase_plan must not inline PlanAutoApprovedDetail literals in transition paths"
        );

        let phase_review_src = include_str!("phase_review.rs");
        assert!(
            phase_review_src.contains("phase_reason_detail::review_decision_transition("),
            "phase_review must use typed constructor for review decision transitions"
        );
        assert!(
            !phase_review_src.contains("ReviewDecisionTransitionDetail {"),
            "phase_review must not inline ReviewDecisionTransitionDetail literals in transition paths"
        );
    }

    #[test]
    fn control_reason_detail_plan_and_author_paths_use_typed_constructors() {
        let phase_plan_src = include_str!("phase_plan.rs");
        assert!(
            phase_plan_src.contains("phase_reason_detail::plan_actionable_auto_approved("),
            "plan actionable auto-approval transitions must use typed detail constructor"
        );
        assert!(
            phase_plan_src.contains("phase_reason_detail::plan_auto_approved("),
            "plan auto-approved transitions must use typed detail constructor"
        );
        assert!(
            !phase_plan_src.contains("\"entry_reason_code\": \"review_actionable_true\""),
            "phase_plan should not inline review_actionable_true reason_detail JSON"
        );
        assert!(
            !phase_plan_src.contains("\"auto_approved_in_agent_mode\": true"),
            "phase_plan should not inline auto_approved reason_detail JSON"
        );

        let plan_helpers_src = include_str!("plan_review_helpers.rs");
        assert!(
            plan_helpers_src.contains("AuthoringCompleteReasonDetail"),
            "authoring-complete transition detail must use typed constructor"
        );
        assert!(
            plan_helpers_src.contains("PlanPrunedEmptyDetail"),
            "plan-pruned-empty transition detail must use typed constructor"
        );
        assert!(
            plan_helpers_src.contains("PlanSemanticInvalidErrorsDetail"),
            "plan semantic-invalid transition detail must use typed constructor"
        );
    }
}
