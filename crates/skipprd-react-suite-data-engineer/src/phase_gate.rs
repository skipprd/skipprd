use crate::domain_types::GuardBlockKind;

use crate::control_flow::Phase;
use crate::progress_controller::{ExecutionState, RepairStatus, MAX_REPAIR_CYCLES};

pub type PreTurnDirective = react_core::workflow::PreTurnDirective<GuardBlockKind>;

fn guard_reason(signal: &str, ctx: &[(&str, &str)]) -> String {
    use std::fmt::Write;
    let mut buf = format!("guard_block: {signal}");
    for (k, v) in ctx {
        write!(buf, " | {k}={v}").ok();
    }
    buf
}

pub fn evaluate_pre_turn_directive(
    execution_state: &ExecutionState,
    phase: Phase,
    max_replan_backtracks: usize,
) -> PreTurnDirective {
    let phase_state = execution_state.phase_state();
    let repair_state = execution_state.repair_state();

    if matches!(repair_state.status, RepairStatus::Exhausted { .. })
        && matches!(phase, Phase::CleanseAuthor | Phase::ModelAuthor)
    {
        return PreTurnDirective::FailFast {
            kind: GuardBlockKind::AuthoringToValidate,
            reason: guard_reason(
                "repair_subroutine_exhausted",
                &[
                    ("cycles_used", &repair_state.cycle_count().to_string()),
                    ("phase", phase.as_str()),
                ],
            ),
        };
    }

    if repair_state.cycle_count() >= MAX_REPAIR_CYCLES
        && matches!(phase, Phase::CleanseAuthor | Phase::ModelAuthor)
    {
        return PreTurnDirective::FailFast {
            kind: GuardBlockKind::AuthoringToValidate,
            reason: guard_reason(
                "repair_cycles_exhausted",
                &[
                    ("repair_cycles", &repair_state.cycle_count().to_string()),
                    ("max_repair_cycles", &MAX_REPAIR_CYCLES.to_string()),
                    ("phase", phase.as_str()),
                ],
            ),
        };
    }

    if phase_state.replan_backtracks >= max_replan_backtracks {
        return PreTurnDirective::FailFast {
            kind: GuardBlockKind::BatchLocked,
            reason: guard_reason(
                "replan_backtrack_limit",
                &[
                    (
                        "replan_backtracks",
                        &phase_state.replan_backtracks.to_string(),
                    ),
                    ("limit", &max_replan_backtracks.to_string()),
                    ("phase", phase.as_str()),
                    (
                        "action",
                        "inspect validate/review errors and apply a targeted fix",
                    ),
                ],
            ),
        };
    }

    PreTurnDirective::Proceed
}

pub fn patch_impl_intent_unsatisfied(execution_state: &ExecutionState, phase: Phase) -> bool {
    let repair = execution_state.repair_state();
    let Some(intent) = repair.pending_patch_impl.as_ref() else {
        return false;
    };
    intent.phase == phase && !intent.mutated_since_set
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress_controller::RepairStatus;

    #[test]
    fn preturn_gate_repair_cycles_exhausted() {
        let mut st = ExecutionState::new();
        st.repair.status = RepairStatus::Pending {
            cycle: MAX_REPAIR_CYCLES,
        };
        let d = evaluate_pre_turn_directive(&st, Phase::CleanseAuthor, 3);
        match d {
            PreTurnDirective::FailFast { kind, .. } => {
                assert_eq!(kind, GuardBlockKind::AuthoringToValidate)
            }
            _ => panic!("expected failfast"),
        }
    }

    #[test]
    fn set_patch_impl_intent_sets_intent() {
        let mut st = ExecutionState::new();
        st.set_pending_patch_impl_intent(Phase::CleanseAuthor);
        assert!(st.repair.pending_patch_impl.is_some());
        assert!(
            !st.repair
                .pending_patch_impl
                .as_ref()
                .unwrap()
                .mutated_since_set
        );
    }

    #[test]
    fn preturn_gate_replan_backtrack_failfast() {
        let mut st = ExecutionState::new();
        st.phase.replan_backtracks = 4;
        let d = evaluate_pre_turn_directive(&st, Phase::ModelPlan, 3);
        match d {
            PreTurnDirective::FailFast { kind, reason } => {
                assert_eq!(kind, GuardBlockKind::BatchLocked);
                assert!(reason.contains("replan_backtrack_limit"));
            }
            _ => panic!("expected failfast"),
        }
    }

    #[test]
    fn preturn_gate_table_driven_precedence() {
        struct Case {
            name: &'static str,
            phase: Phase,
            setup: fn(&mut ExecutionState),
            expect_fail: bool,
            expect_kind: Option<GuardBlockKind>,
        }
        let cases = vec![
            Case {
                name: "default_proceed",
                phase: Phase::ModelPlan,
                setup: |_| {},
                expect_fail: false,
                expect_kind: None,
            },
            Case {
                name: "repair_cycles_has_priority_over_replan",
                phase: Phase::ModelAuthor,
                setup: |st| {
                    st.repair.status = RepairStatus::Pending {
                        cycle: MAX_REPAIR_CYCLES,
                    };
                    st.phase.replan_backtracks = 10;
                },
                expect_fail: true,
                expect_kind: Some(GuardBlockKind::AuthoringToValidate),
            },
            Case {
                name: "replan_failfast_when_no_stall",
                phase: Phase::ModelPlan,
                setup: |st| {
                    st.phase.replan_backtracks = 5;
                },
                expect_fail: true,
                expect_kind: Some(GuardBlockKind::BatchLocked),
            },
            Case {
                name: "repair_cycles_ignored_on_non_author_phase",
                phase: Phase::ModelPlan,
                setup: |st| {
                    st.repair.status = RepairStatus::Pending {
                        cycle: MAX_REPAIR_CYCLES,
                    };
                },
                expect_fail: false,
                expect_kind: None,
            },
            Case {
                name: "repair_exhausted_has_priority_over_replan",
                phase: Phase::CleanseAuthor,
                setup: |st| {
                    st.repair.status = RepairStatus::Exhausted { cycles_used: 1 };
                    st.phase.replan_backtracks = 10;
                },
                expect_fail: true,
                expect_kind: Some(GuardBlockKind::AuthoringToValidate),
            },
        ];

        for c in cases {
            let mut st = ExecutionState::new();
            (c.setup)(&mut st);
            let d = evaluate_pre_turn_directive(&st, c.phase, 3);
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
    fn preturn_gate_repair_exhausted_failfast() {
        let mut st = ExecutionState::new();
        st.repair.status = RepairStatus::Exhausted { cycles_used: 2 };
        let d = evaluate_pre_turn_directive(&st, Phase::ModelAuthor, 3);
        match d {
            PreTurnDirective::FailFast { kind, reason } => {
                assert_eq!(kind, GuardBlockKind::AuthoringToValidate);
                assert!(reason.contains("repair_subroutine_exhausted"));
            }
            _ => panic!("expected failfast for Exhausted status"),
        }
    }

    #[test]
    fn preturn_gate_repair_exhausted_ignored_on_non_author_phase() {
        let mut st = ExecutionState::new();
        st.repair.status = RepairStatus::Exhausted { cycles_used: 2 };
        let d = evaluate_pre_turn_directive(&st, Phase::ModelPlan, 3);
        assert!(matches!(d, PreTurnDirective::Proceed));
    }

    #[test]
    fn control_critical_plan_and_review_reason_details_use_typed_constructors() {
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
        let plan_helpers_src = include_str!("plan_review_helpers.rs");
        assert!(
            plan_helpers_src.contains("AuthoringCompleteReasonDetail"),
            "authoring-complete transition detail must use typed constructor"
        );
        assert!(
            plan_helpers_src.contains("PhaseTransition::PlanPrunedEmpty"),
            "plan-pruned-empty must use PhaseTransition::PlanPrunedEmpty variant"
        );
        assert!(
            plan_helpers_src.contains("PhaseTransition::PlanSemanticInvalid"),
            "plan semantic-invalid must use PhaseTransition::PlanSemanticInvalid variant"
        );
    }
}
