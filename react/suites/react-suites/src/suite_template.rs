//! Template for building new workflow-oriented suites on top of react-core contracts.
//!
//! Copy this module when adding a new suite and replace the placeholder types.
//! The goal is to make extension points explicit and compile-time checked.

use react_core::suite::{WorkflowPolicy, WorkflowSuiteContract};
use react_core::workflow::PreTurnDirective;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TemplatePhase {
    Intake,
    Plan,
    Execute,
    Review,
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TemplateReason {
    PhaseSet,
    PlanApproved,
    ValidateFail,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TemplateState {
    pub phase: Option<TemplatePhase>,
    pub replan_backtracks: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TemplateEvent {
    Enter(TemplatePhase),
    Loopback,
}

pub struct TemplateWorkflowContract;

impl WorkflowSuiteContract for TemplateWorkflowContract {
    type Phase = TemplatePhase;
    type ReasonCode = TemplateReason;
    type State = TemplateState;
    type Event = TemplateEvent;

    fn phase_as_str(phase: Self::Phase) -> &'static str {
        match phase {
            TemplatePhase::Intake => "intake",
            TemplatePhase::Plan => "plan",
            TemplatePhase::Execute => "execute",
            TemplatePhase::Review => "review",
            TemplatePhase::Done => "done",
        }
    }

    fn reason_as_str(reason: Self::ReasonCode) -> &'static str {
        match reason {
            TemplateReason::PhaseSet => "phase_set",
            TemplateReason::PlanApproved => "plan_approved",
            TemplateReason::ValidateFail => "validate_fail",
        }
    }

    fn is_backtrack(from: Self::Phase, to: Self::Phase) -> bool {
        matches!(from, TemplatePhase::Review) && matches!(to, TemplatePhase::Plan | TemplatePhase::Execute)
    }

    fn replan_backtrack_cap() -> usize {
        3
    }
}

pub struct TemplateWorkflowPolicy;

impl WorkflowPolicy<TemplateWorkflowContract> for TemplateWorkflowPolicy {
    fn pre_turn(&self, state: &TemplateState) -> PreTurnDirective {
        if state.replan_backtracks >= TemplateWorkflowContract::replan_backtrack_cap() {
            return PreTurnDirective::FailFast {
                kind: react_core::control_flow::GuardBlockKind::BatchLocked,
                reason: "loopback cap reached".to_string(),
            };
        }
        PreTurnDirective::Proceed
    }

    fn reduce(&self, state: &mut TemplateState, event: TemplateEvent) {
        match event {
            TemplateEvent::Enter(p) => state.phase = Some(p),
            TemplateEvent::Loopback => {
                state.replan_backtracks = state.replan_backtracks.saturating_add(1)
            }
        }
    }
}

// Domain checklist for new suites:
// - Legal framework: configure rule-evidence phases and policy checks.
// - Personal assistant: configure task queue + approval gates for side-effect actions.
// - Lead gen: configure entity-level pipeline stages and outbound policy boundaries.
