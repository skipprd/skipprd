use react_core::control_flow::PhaseReasonCode;
use react_core::session::{Observation, ThreadStep, ThreadStore};
use serde_json::Value;

use crate::data_engineer::control_flow::{
    allowed_next_phases, is_annotation_reason, is_cleanse_replan_backtrack,
    is_model_replan_backtrack, replan_backtrack_counter_cap, Phase,
};
use crate::data_engineer::progress_controller::ExecutionState;

pub async fn dispatch_phase_transition(
    store: &ThreadStore,
    thread_id: &str,
    agent: Option<String>,
    from_phase: Option<Phase>,
    phase: Phase,
    reason_code: Option<PhaseReasonCode>,
    reason_detail: Option<Value>,
) -> Result<(), String> {
    if let Some(from) = from_phase {
        let is_same_phase_annotation = from == phase && is_annotation_reason(reason_code);
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

    let agent = agent.unwrap_or_else(|| "unknown".to_string());
    store
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
        .await?;

    // Canonical transition side effects are centralized here.
    let mut st = ExecutionState::load(store, thread_id)
        .await
        .unwrap_or_else(ExecutionState::new);
    if let Some(from) = from_phase {
        let is_reset_reason = phase == Phase::Done
            || matches!(
                reason_code,
                Some(
                    PhaseReasonCode::ValidatePass
                        | PhaseReasonCode::PlanTasksDone
                        | PhaseReasonCode::NoWorkAllDone
                        | PhaseReasonCode::PublishSuccess
                        | PhaseReasonCode::PublishConfirmedSuccess
                )
            );
        let is_backtrack =
            is_cleanse_replan_backtrack(from, phase) || is_model_replan_backtrack(from, phase);
        if is_reset_reason {
            st.replan_backtracks = 0;
        } else if is_backtrack {
            st.replan_backtracks = st
                .replan_backtracks
                .saturating_add(1)
                .min(replan_backtrack_counter_cap());
        }
    }
    st.current_phase = Some(phase);
    st.phase_reason_code = reason_code;
    st.save(store, thread_id).await?;

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
    async fn validate_pass_backtrack_resets_counter_instead_of_incrementing() {
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
            Phase::CleanseAuthor,
            Some(PhaseReasonCode::ValidatePass),
            None,
        )
        .await
        .expect("transition should succeed");

        let got = ExecutionState::load(&store, tid).await.expect("state should load");
        assert_eq!(
            got.replan_backtracks, 0,
            "validate_pass transitions must reset loopback counter"
        );
    }
}
