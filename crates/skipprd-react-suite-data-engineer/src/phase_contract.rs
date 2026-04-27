use crate::domain_types::GuardBlockKind;
use crate::progress_controller::PhaseTransition;
use react_core::session::ThreadStore;

use crate::control_flow::{Phase, TransitionIntent};

#[derive(Clone, Debug)]
pub struct PhaseDecision {
    pub to: Phase,
    pub intent: TransitionIntent,
    pub transition: Option<PhaseTransition>,
}

impl PhaseDecision {
    pub fn forward(to: Phase, transition: Option<PhaseTransition>) -> Self {
        Self {
            to,
            intent: TransitionIntent::Forward,
            transition,
        }
    }

    pub fn loopback(to: Phase, transition: Option<PhaseTransition>) -> Self {
        Self {
            to,
            intent: TransitionIntent::Loopback,
            transition,
        }
    }

    pub fn annotation(phase: Phase, transition: Option<PhaseTransition>) -> Self {
        Self {
            to: phase,
            intent: TransitionIntent::Annotation,
            transition,
        }
    }
}

pub async fn commit_phase_decision(
    thread_store: &ThreadStore,
    thread_id: &str,
    from_phase: Option<Phase>,
    decision: PhaseDecision,
) -> Result<(), String> {
    let PhaseDecision {
        to,
        intent,
        transition,
    } = decision;
    crate::transition_dispatcher::dispatch_phase_transition(
        thread_store,
        thread_id,
        Some(crate::env_util::DEFAULT_AGENT_NAME.to_string()),
        from_phase,
        to,
        intent,
        transition,
    )
    .await
    .map_err(|e| e.to_string())
}

pub async fn commit_metered_decision(
    thread_store: &ThreadStore,
    thread_id: &str,
    from_phase: Option<Phase>,
    decision: PhaseDecision,
    usage: Vec<crate::metering::UsageEvent>,
    metering: &crate::metering::MeteringClient,
) -> Result<(), String> {
    if let Err(e) = metering.record_batch(&usage).await {
        let _ = commit_guard_block(
            thread_store,
            thread_id,
            decision.to,
            GuardBlockKind::PrecheckFailed,
            format!("Credits exhausted: {e}"),
        )
        .await;
        return Err(e);
    }
    commit_phase_decision(thread_store, thread_id, from_phase, decision).await
}

pub async fn commit_guard_block(
    thread_store: &ThreadStore,
    thread_id: &str,
    phase: Phase,
    kind: GuardBlockKind,
    reason: impl Into<String>,
) -> Result<(), String> {
    crate::transition_dispatcher::apply_phase_directive(
        thread_store,
        thread_id,
        Some(crate::env_util::DEFAULT_AGENT_NAME.to_string()),
        Some(phase),
        crate::transition_dispatcher::PhaseDirective::Block {
            phase,
            kind,
            reason: reason.into(),
        },
    )
    .await
    .map_err(|e| e.to_string())
}

pub async fn commit_plan_revision_loopback(
    thread_store: &ThreadStore,
    thread_id: &str,
    from_phase: Phase,
    violations: Vec<crate::progress_controller::PlanViolation>,
    strategy: crate::progress_controller::PlanRevisionStrategy,
) -> Result<(), String> {
    let track = crate::track_spec::TrackKind::from_any_phase(from_phase).ok_or_else(|| {
        format!(
            "PlanRevisionRequested from phase '{}' which has no associated plan track",
            from_phase.as_str()
        )
    })?;
    crate::state_manager::apply_execution_event(
        &thread_store.control_store(),
        thread_id,
        crate::progress_controller::DataEngineerEvent::PlanRevisionRequested {
            violations,
            strategy,
        },
    )
    .await
    .map(|_| ())
    .map_err(|e| e.to_string())?;
    commit_phase_decision(
        thread_store,
        thread_id,
        Some(from_phase),
        PhaseDecision::loopback(
            track.plan_phase(),
            Some(PhaseTransition::PlanRevisionRequested {
                violations: vec![],
                strategy: crate::progress_controller::PlanRevisionStrategy::Rewrite,
            }),
        ),
    )
    .await
}
