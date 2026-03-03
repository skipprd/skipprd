use react_core::control_flow::PhaseReasonCode;
use react_core::session::ThreadStore;

use crate::data_engineer::control_flow::{Phase, TransitionIntent};
use crate::data_engineer::phase_actions::apply_phase_transition;

#[derive(Clone, Debug)]
pub enum PhaseDecision {
    Stay,
    Transition {
        to: Phase,
        intent: TransitionIntent,
        reason_code: Option<PhaseReasonCode>,
        reason_detail: Option<serde_json::Value>,
    },
}

impl PhaseDecision {
    pub fn forward(
        to: Phase,
        reason_code: Option<PhaseReasonCode>,
        reason_detail: Option<serde_json::Value>,
    ) -> Self {
        Self::Transition {
            to,
            intent: TransitionIntent::Forward,
            reason_code,
            reason_detail,
        }
    }

    pub fn loopback(
        to: Phase,
        reason_code: Option<PhaseReasonCode>,
        reason_detail: Option<serde_json::Value>,
    ) -> Self {
        Self::Transition {
            to,
            intent: TransitionIntent::Loopback,
            reason_code,
            reason_detail,
        }
    }

    pub fn annotation(
        phase: Phase,
        reason_code: Option<PhaseReasonCode>,
        reason_detail: Option<serde_json::Value>,
    ) -> Self {
        Self::Transition {
            to: phase,
            intent: TransitionIntent::Annotation,
            reason_code,
            reason_detail,
        }
    }
}

pub async fn commit_phase_decision(
    thread_store: &ThreadStore,
    thread_id: &str,
    from_phase: Option<Phase>,
    decision: PhaseDecision,
) -> Result<(), String> {
    match decision {
        PhaseDecision::Stay => Ok(()),
        PhaseDecision::Transition {
            to,
            intent,
            reason_code,
            reason_detail,
        } => {
            apply_phase_transition(
                thread_store,
                thread_id,
                from_phase,
                to,
                intent,
                reason_code,
                reason_detail,
            )
            .await
        }
    }
}
