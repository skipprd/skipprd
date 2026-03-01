use react_core::control_flow::PhaseReasonCode;
use react_core::session::{Observation, ThreadStep, ThreadStore};
use serde_json::Value;

use crate::data_engineer::control_flow::{
    allowed_next_phases, is_annotation_reason, is_cleanse_replan_backtrack,
    is_model_replan_backtrack, replan_backtrack_counter_cap, Phase, TransitionIntent,
};
use crate::data_engineer::progress_controller::ExecutionState;

pub async fn dispatch_phase_transition(
    store: &ThreadStore,
    thread_id: &str,
    agent: Option<String>,
    from_phase: Option<Phase>,
    phase: Phase,
    intent: TransitionIntent,
    reason_code: Option<PhaseReasonCode>,
    reason_detail: Option<Value>,
) -> Result<(), String> {
    if let Some(from) = from_phase {
        let is_same_phase_annotation = intent == TransitionIntent::Annotation
            || (from == phase && is_annotation_reason(reason_code));
        if !is_same_phase_annotation && !allowed_next_phases(from).contains(&phase) {
            return Err(format!(
                "invalid_phase_transition: from='{}' to='{}' reason='{}'",
                from.as_str(),
                phase.as_str(),
                reason_code
                    .map(|c| c.as_str().to_string())
                    .unwrap_or_else(|| "none".to_string())
            ));
        }
    }

    // Canonical transition side effects are centralized here.
    let mut st = ExecutionState::load(store, thread_id)
        .await
        .unwrap_or_else(ExecutionState::new);
    let prev_state = st.clone();
    if let Some(from) = from_phase {
        let is_backtrack =
            is_cleanse_replan_backtrack(from, phase) || is_model_replan_backtrack(from, phase);
        match intent {
            TransitionIntent::Annotation => {}
            TransitionIntent::Forward => {
                st.replan_backtracks = 0;
            }
            TransitionIntent::Loopback => {
                if is_backtrack {
                    st.replan_backtracks = st
                        .replan_backtracks
                        .saturating_add(1)
                        .min(replan_backtrack_counter_cap());
                }
            }
        }
    }
    st.current_phase = Some(phase);
    st.phase_reason_code = reason_code;
    st.save(store, thread_id).await?;

    let agent = agent.unwrap_or_else(|| "unknown".to_string());
    if let Err(e) = store
        .append_step(
            thread_id,
            ThreadStep::Phase {
                phase: phase.as_str().to_string(),
                from_phase: from_phase.map(|p| p.as_str().to_string()),
                reason_code,
                reason_detail,
                observation: Observation::ok(),
                ts: chrono::Utc::now().to_rfc3339(),
                agent,
            },
        )
        .await
    {
        // Best-effort rollback to avoid control-state/log divergence.
        let _ = prev_state.save(store, thread_id).await;
        return Err(e);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use react_core::keyspace::DefaultKeyspace;
    use react_core::scope::RequestScope;
    use react_core::storage::InMemoryStorageAdapter;
    use std::sync::Arc;

    #[tokio::test]
    async fn validate_pass_to_review_resets_counter() {
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let scope = RequestScope {
            tenant: "t".into(),
            workspace: "w".into(),
            project_id: "p".into(),
        };
        let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
        let store = ThreadStore::new(storage, scope, keyspace);
        let tid = "tid-validate-pass-reset";

        let mut st = ExecutionState::new();
        st.replan_backtracks = 2;
        st.save(&store, tid).await.expect("seed execution state");

        dispatch_phase_transition(
            &store,
            tid,
            Some("agent".to_string()),
            Some(Phase::CleanseValidate),
            Phase::CleanseReview,
            TransitionIntent::Forward,
            Some(PhaseReasonCode::ValidatePassToReview),
            None,
        )
        .await
        .expect("transition should succeed");

        let got = ExecutionState::load(&store, tid).await.expect("state should load");
        assert_eq!(
            got.replan_backtracks, 0,
            "forward transitions must reset loopback counter"
        );
    }

    #[tokio::test]
    async fn validate_pass_to_author_increments_loopback_counter() {
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let scope = RequestScope {
            tenant: "t".into(),
            workspace: "w".into(),
            project_id: "p".into(),
        };
        let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
        let store = ThreadStore::new(storage, scope, keyspace);
        let tid = "tid-validate-pass-loopback";

        let mut st = ExecutionState::new();
        st.replan_backtracks = 0;
        st.save(&store, tid).await.expect("seed execution state");

        dispatch_phase_transition(
            &store,
            tid,
            Some("agent".to_string()),
            Some(Phase::CleanseValidate),
            Phase::CleanseAuthor,
            TransitionIntent::Loopback,
            Some(PhaseReasonCode::ValidatePassToAuthoring),
            None,
        )
        .await
        .expect("transition should succeed");

        let got = ExecutionState::load(&store, tid).await.expect("state should load");
        assert_eq!(got.replan_backtracks, 1);
    }
}
