use react_core::control_flow::{GuardBlockKind, PhaseReasonCode};
use react_core::session::ThreadStore;

use crate::data_engineer::control_flow;

pub fn plan_status_reason_detail<S: std::fmt::Debug>(status: S) -> serde_json::Value {
    serde_json::json!({ "status": format!("{status:?}") })
}

pub async fn apply_phase_transition(
    thread_store: &ThreadStore,
    thread_id: &str,
    from_phase: Option<control_flow::Phase>,
    to_phase: control_flow::Phase,
    intent: control_flow::TransitionIntent,
    reason_code: Option<PhaseReasonCode>,
    reason_detail: Option<serde_json::Value>,
) -> Result<(), String> {
    crate::data_engineer::transition_dispatcher::apply_phase_directive(
        thread_store,
        thread_id,
        Some("agent".to_string()),
        from_phase,
        crate::data_engineer::transition_dispatcher::PhaseDirective::Transition {
            to: to_phase,
            intent,
            reason_code,
            reason_detail,
        },
    )
    .await
}

pub async fn apply_guard_block(
    thread_store: &ThreadStore,
    thread_id: &str,
    phase: control_flow::Phase,
    kind: GuardBlockKind,
    reason: impl Into<String>,
) -> Result<(), String> {
    let reason = reason.into();
    crate::data_engineer::transition_dispatcher::apply_phase_directive(
        thread_store,
        thread_id,
        Some("agent".to_string()),
        Some(phase),
        crate::data_engineer::transition_dispatcher::PhaseDirective::Block {
            phase,
            kind,
            reason,
        },
    )
    .await
}

