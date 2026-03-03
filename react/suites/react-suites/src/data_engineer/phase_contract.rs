use react_core::control_flow::{GuardBlockKind, PhaseReasonCode};
use react_core::session::ThreadStore;

use crate::data_engineer::control_flow::{Phase, TransitionIntent};

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
        } => crate::data_engineer::transition_dispatcher::apply_phase_directive(
            thread_store,
            thread_id,
            Some("agent".to_string()),
            from_phase,
            crate::data_engineer::transition_dispatcher::PhaseDirective::Transition {
                to,
                intent,
                reason_code,
                reason_detail,
            },
        )
        .await,
    }
}

pub async fn commit_guard_block(
    thread_store: &ThreadStore,
    thread_id: &str,
    phase: Phase,
    kind: GuardBlockKind,
    reason: impl Into<String>,
) -> Result<(), String> {
    crate::data_engineer::transition_dispatcher::apply_phase_directive(
        thread_store,
        thread_id,
        Some("agent".to_string()),
        Some(phase),
        crate::data_engineer::transition_dispatcher::PhaseDirective::Block {
            phase,
            kind,
            reason: reason.into(),
        },
    )
    .await
}

pub fn plan_status_reason_detail<S: std::fmt::Debug>(status: S) -> serde_json::Value {
    serde_json::json!({ "status": format!("{status:?}") })
}
