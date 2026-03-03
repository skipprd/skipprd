use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::control_flow::GuardBlockKind;
use crate::suite::{WorkflowNodeContract, WorkflowSuiteContract};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitionIntent {
    Annotation,
    Forward,
    Loopback,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PhaseDirective<P, R> {
    Transition {
        to: P,
        intent: TransitionIntent,
        reason_code: Option<R>,
        reason_detail: Option<Value>,
    },
    Annotate {
        phase: P,
        reason_code: R,
        reason_detail: Option<Value>,
    },
    Block {
        phase: P,
        kind: GuardBlockKind,
        reason: String,
    },
    Stay,
}

pub trait TypedReasonDetail: Serialize {}

impl<T: Serialize> TypedReasonDetail for T {}

pub fn reason_detail_value<T: TypedReasonDetail>(detail: &T) -> Value {
    serde_json::to_value(detail).unwrap_or(Value::Null)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreTurnDirective {
    Proceed,
    FailFast {
        kind: GuardBlockKind,
        reason: String,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PreTurnStateSnapshot {
    pub mode_is_mutate: bool,
    pub stall_count: usize,
    pub max_stall_count: usize,
    pub replan_backtracks: usize,
    pub hard_mutation_repair_mode: bool,
    pub ladder_stop: bool,
    pub target_path: Option<String>,
    pub attempt_count: usize,
}

pub fn evaluate_pre_turn_directive(
    st: &PreTurnStateSnapshot,
    max_replan_backtracks: usize,
) -> PreTurnDirective {
    if st.mode_is_mutate && st.stall_count >= st.max_stall_count {
        return PreTurnDirective::FailFast {
            kind: GuardBlockKind::AuthoringToValidate,
            reason: format!(
                "failed to make progress for this thread: execution_state stall_count={} reached max_stall_count={} in mode=mutate",
                st.stall_count, st.max_stall_count
            ),
        };
    }

    if st.replan_backtracks >= max_replan_backtracks {
        return PreTurnDirective::FailFast {
            kind: GuardBlockKind::BatchLocked,
            reason: format!(
                "failed to make progress for this thread: observed {} validate/review loopback(s) since the last successful validate (limit {}).",
                st.replan_backtracks, max_replan_backtracks
            ),
        };
    }

    if st.hard_mutation_repair_mode && st.ladder_stop {
        let target = st.target_path.as_deref().unwrap_or("(unknown target)");
        return PreTurnDirective::FailFast {
            kind: GuardBlockKind::AuthoringToValidate,
            reason: format!(
                "failed to make progress for this thread: deterministic repair ladder reached stop for '{}' after {} attempt(s). Apply a manual fix and rerun.",
                target.trim(),
                st.attempt_count
            ),
        };
    }

    PreTurnDirective::Proceed
}

pub fn next_replan_backtracks(
    current: usize,
    intent: TransitionIntent,
    is_backtrack: bool,
    cap: usize,
) -> usize {
    match intent {
        TransitionIntent::Annotation => current,
        TransitionIntent::Forward => 0,
        TransitionIntent::Loopback => {
            if is_backtrack {
                current.saturating_add(1).min(cap)
            } else {
                current
            }
        }
    }
}

/// Executes suite-defined pre-turn guard logic through the typed workflow contract.
pub fn evaluate_pre_turn<C: WorkflowSuiteContract>(state: &C::State) -> PreTurnDirective {
    C::pre_turn(state)
}

/// Applies a typed workflow event through the suite reducer contract.
pub fn reduce_event<C: WorkflowSuiteContract>(state: &mut C::State, event: C::Event) {
    C::reduce(state, event);
}

/// Derives the canonical phase from the suite's typed workflow node.
pub fn phase_from_state<C: WorkflowNodeContract>(state: &C::State) -> C::Phase {
    let node = C::node_from_state(state);
    C::phase_from_node(node)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suite::{WorkflowNodeContract, WorkflowSuiteContract};

    #[test]
    fn preturn_stall_failfast() {
        let st = PreTurnStateSnapshot {
            mode_is_mutate: true,
            stall_count: 3,
            max_stall_count: 3,
            ..Default::default()
        };
        match evaluate_pre_turn_directive(&st, 3) {
            PreTurnDirective::FailFast { kind, .. } => {
                assert_eq!(kind, GuardBlockKind::AuthoringToValidate);
            }
            _ => panic!("expected failfast"),
        }
    }

    #[test]
    fn loopback_counter_rules() {
        assert_eq!(next_replan_backtracks(2, TransitionIntent::Annotation, true, 5), 2);
        assert_eq!(next_replan_backtracks(2, TransitionIntent::Forward, true, 5), 0);
        assert_eq!(next_replan_backtracks(2, TransitionIntent::Loopback, false, 5), 2);
        assert_eq!(next_replan_backtracks(2, TransitionIntent::Loopback, true, 3), 3);
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum DummyPhase {
        A,
        B,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum DummyReason {
        X,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum DummyNode {
        A,
        B,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct DummyState {
        node: DummyNode,
        blocked: bool,
        count: usize,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum DummyEvent {
        Advance,
    }

    struct DummyContract;

    impl WorkflowSuiteContract for DummyContract {
        type Phase = DummyPhase;
        type ReasonCode = DummyReason;
        type State = DummyState;
        type Event = DummyEvent;

        fn phase_as_str(_phase: Self::Phase) -> &'static str {
            "dummy"
        }
        fn reason_as_str(_reason: Self::ReasonCode) -> &'static str {
            "dummy_reason"
        }
        fn is_backtrack(_from: Self::Phase, _to: Self::Phase) -> bool {
            false
        }
        fn replan_backtrack_cap() -> usize {
            1
        }
        fn pre_turn(state: &Self::State) -> PreTurnDirective {
            if state.blocked {
                PreTurnDirective::FailFast {
                    kind: GuardBlockKind::BatchLocked,
                    reason: "blocked".to_string(),
                }
            } else {
                PreTurnDirective::Proceed
            }
        }
        fn reduce(state: &mut Self::State, event: Self::Event) {
            match event {
                DummyEvent::Advance => {
                    state.node = DummyNode::B;
                    state.count += 1;
                }
            }
        }
    }

    impl WorkflowNodeContract for DummyContract {
        type Node = DummyNode;

        fn node_from_state(state: &Self::State) -> Self::Node {
            state.node
        }
        fn phase_from_node(node: Self::Node) -> Self::Phase {
            match node {
                DummyNode::A => DummyPhase::A,
                DummyNode::B => DummyPhase::B,
            }
        }
    }

    #[test]
    fn contract_helpers_route_through_typed_contract() {
        let mut st = DummyState {
            node: DummyNode::A,
            blocked: false,
            count: 0,
        };
        assert_eq!(phase_from_state::<DummyContract>(&st), DummyPhase::A);
        assert!(matches!(
            evaluate_pre_turn::<DummyContract>(&st),
            PreTurnDirective::Proceed
        ));
        reduce_event::<DummyContract>(&mut st, DummyEvent::Advance);
        assert_eq!(phase_from_state::<DummyContract>(&st), DummyPhase::B);
        assert_eq!(st.count, 1);
    }
}
