use react_core::control_flow::PhaseReasonCode;
use react_core::session::{Observation, ThreadStep, ThreadStore};
use serde_json::Value;

use crate::data_engineer::control_flow::{
    allowed_next_phases, is_annotation_reason, is_cleanse_replan_backtrack,
    is_model_replan_backtrack, replan_backtrack_counter_cap, Phase, TransitionIntent,
};
use crate::data_engineer::progress_controller::ExecutionState;
use crate::data_engineer::state_manager;

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
    let mut st = state_manager::load_execution_state(store, thread_id)
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
    if phase == Phase::ModelPlan && from_phase != Some(Phase::ModelPlan) {
        // Scope manifest retry suppression to a single model-plan attempt.
        st.reset_manifest_lookup_state();
    }
    if matches!(phase, Phase::CleansePlan | Phase::ModelPlan) && from_phase != Some(phase) {
        st.reset_plan_bootstrap(phase);
    }
    st.current_phase = Some(phase);
    st.phase_reason_code = reason_code;
    st.phase_reason_detail = reason_detail.clone();
    state_manager::save_execution_state(store, thread_id, &st).await?;

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
        let _ = state_manager::save_execution_state(store, thread_id, &prev_state).await;
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
        state_manager::save_execution_state(&store, tid, &st)
            .await
            .expect("seed execution state");

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

        let got = state_manager::load_execution_state(&store, tid).await.expect("state should load");
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
        state_manager::save_execution_state(&store, tid, &st)
            .await
            .expect("seed execution state");

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

        let got = state_manager::load_execution_state(&store, tid).await.expect("state should load");
        assert_eq!(got.replan_backtracks, 1);
    }

    #[tokio::test]
    async fn entering_model_plan_resets_manifest_and_bootstrap_state() {
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let scope = RequestScope {
            tenant: "t".into(),
            workspace: "w".into(),
            project_id: "p".into(),
        };
        let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
        let store = ThreadStore::new(storage, scope, keyspace);
        let tid = "tid-model-plan-reset-state";

        let mut st = ExecutionState::new();
        st.model_plan_bootstrap_done = true;
        st.manifest_lookup.retry_suppressed = true;
        st.manifest_lookup.repeated_failure_count = 3;
        st.manifest_lookup.failure_signature = Some("NoSuchKey:Ambiguous".to_string());
        state_manager::save_execution_state(&store, tid, &st)
            .await
            .expect("seed execution state");

        dispatch_phase_transition(
            &store,
            tid,
            Some("agent".to_string()),
            Some(Phase::ModelReview),
            Phase::ModelPlan,
            TransitionIntent::Loopback,
            Some(PhaseReasonCode::ReviewPatchPlan),
            None,
        )
        .await
        .expect("transition should succeed");

        let got = state_manager::load_execution_state(&store, tid).await.expect("state should load");
        assert!(
            !got.model_plan_bootstrap_done,
            "model-plan bootstrap should reset on fresh model_plan entry"
        );
        assert!(
            !got.manifest_lookup.retry_suppressed && got.manifest_lookup.repeated_failure_count == 0,
            "manifest lookup retry state should reset on fresh model_plan entry"
        );
    }
}
