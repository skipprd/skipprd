use react_core::control_flow::{GuardBlockKind, PhaseReasonCode};
use react_core::session::{Observation, ThreadStep, ThreadStore};
use serde_json::Value;

use crate::data_engineer::control_flow::{
    allowed_next_phases, is_annotation_reason, is_cleanse_replan_backtrack,
    is_model_replan_backtrack, replan_backtrack_counter_cap, Phase, TransitionIntent,
};
use crate::data_engineer::progress_controller::ExecutionState;
use crate::data_engineer::state_manager;

#[derive(Clone, Debug)]
pub enum PhaseDirective {
    Transition {
        to: Phase,
        intent: TransitionIntent,
        reason_code: Option<PhaseReasonCode>,
        reason_detail: Option<Value>,
    },
    Annotate {
        phase: Phase,
        reason_code: PhaseReasonCode,
        reason_detail: Option<Value>,
    },
    Block {
        phase: Phase,
        kind: GuardBlockKind,
        reason: String,
    },
    Stay,
}

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

pub async fn apply_phase_directive(
    store: &ThreadStore,
    thread_id: &str,
    agent: Option<String>,
    from_phase: Option<Phase>,
    directive: PhaseDirective,
) -> Result<(), String> {
    match directive {
        PhaseDirective::Transition {
            to,
            intent,
            reason_code,
            reason_detail,
        } => {
            dispatch_phase_transition(
                store,
                thread_id,
                agent,
                from_phase,
                to,
                intent,
                reason_code,
                reason_detail,
            )
            .await
        }
        PhaseDirective::Annotate {
            phase,
            reason_code,
            reason_detail,
        } => {
            dispatch_phase_transition(
                store,
                thread_id,
                agent,
                from_phase.or(Some(phase)),
                phase,
                TransitionIntent::Annotation,
                Some(reason_code),
                reason_detail,
            )
            .await
        }
        PhaseDirective::Block {
            phase,
            kind,
            reason,
        } => store
            .append_step(
                thread_id,
                ThreadStep::GuardBlock {
                    phase: phase.as_str().to_string(),
                    kind,
                    reason: reason.clone(),
                    observation: Observation::fail(vec![reason]),
                    ts: chrono::Utc::now().to_rfc3339(),
                    agent: agent.unwrap_or_else(|| "agent".to_string()),
                },
            )
            .await
            .map_err(|e| format!("failed to append guard step: {e}")),
        PhaseDirective::Stay => Ok(()),
    }
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

    #[tokio::test]
    async fn apply_phase_directive_block_appends_guard() {
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let scope = RequestScope {
            tenant: "t".into(),
            workspace: "w".into(),
            project_id: "p".into(),
        };
        let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
        let store = ThreadStore::new(storage, scope, keyspace);
        let tid = "tid-phase-directive-block";

        apply_phase_directive(
            &store,
            tid,
            Some("agent".to_string()),
            Some(Phase::CleanseAuthor),
            PhaseDirective::Block {
                phase: Phase::CleanseAuthor,
                kind: GuardBlockKind::AuthoringToValidate,
                reason: "blocked".to_string(),
            },
        )
        .await
        .expect("directive block should append");

        let log = store.get(tid).await.expect("thread log should exist");
        let steps = log.steps;
        assert!(
            steps.iter().any(|s| matches!(s, ThreadStep::GuardBlock { phase, .. } if phase == "cleanse_author")),
            "guard block must be appended by directive applier"
        );
    }

    #[tokio::test]
    async fn apply_phase_directive_annotate_keeps_backtrack_counter() {
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let scope = RequestScope {
            tenant: "t".into(),
            workspace: "w".into(),
            project_id: "p".into(),
        };
        let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
        let store = ThreadStore::new(storage, scope, keyspace);
        let tid = "tid-phase-directive-annotate";
        let mut st = ExecutionState::new();
        st.current_phase = Some(Phase::CleansePlan);
        st.replan_backtracks = 2;
        state_manager::save_execution_state(&store, tid, &st)
            .await
            .expect("seed state");

        apply_phase_directive(
            &store,
            tid,
            Some("agent".to_string()),
            Some(Phase::CleansePlan),
            PhaseDirective::Annotate {
                phase: Phase::CleansePlan,
                reason_code: PhaseReasonCode::PhaseSet,
                reason_detail: Some(serde_json::json!({"note":"x"})),
            },
        )
        .await
        .expect("annotation should succeed");

        let got = state_manager::load_execution_state(&store, tid)
            .await
            .expect("state");
        assert_eq!(got.replan_backtracks, 2);
    }

    #[tokio::test]
    async fn loopback_counter_saturates_at_cap() {
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let scope = RequestScope {
            tenant: "t".into(),
            workspace: "w".into(),
            project_id: "p".into(),
        };
        let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
        let store = ThreadStore::new(storage, scope, keyspace);
        let tid = "tid-loopback-cap";
        let mut st = ExecutionState::new();
        st.current_phase = Some(Phase::CleanseValidate);
        st.replan_backtracks = replan_backtrack_counter_cap();
        state_manager::save_execution_state(&store, tid, &st)
            .await
            .expect("seed state");

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

        let got = state_manager::load_execution_state(&store, tid)
            .await
            .expect("state");
        assert_eq!(got.replan_backtracks, replan_backtrack_counter_cap());
    }

    #[tokio::test]
    async fn apply_phase_directive_transition_and_stay_are_handled() {
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let scope = RequestScope {
            tenant: "t".into(),
            workspace: "w".into(),
            project_id: "p".into(),
        };
        let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
        let store = ThreadStore::new(storage, scope, keyspace);
        let tid = "tid-phase-directive-stay";

        apply_phase_directive(
            &store,
            tid,
            Some("agent".to_string()),
            None,
            PhaseDirective::Stay,
        )
        .await
        .expect("stay should succeed");
        assert!(
            store.get(tid).await.is_err(),
            "stay should not append thread steps"
        );

        apply_phase_directive(
            &store,
            tid,
            Some("agent".to_string()),
            Some(Phase::Preflight),
            PhaseDirective::Transition {
                to: Phase::CleansePlan,
                intent: TransitionIntent::Forward,
                reason_code: Some(PhaseReasonCode::PreflightOk),
                reason_detail: None,
            },
        )
        .await
        .expect("transition should succeed");

        let log = store.get(tid).await.expect("thread log should exist");
        assert!(
            log.steps
                .iter()
                .any(|s| matches!(s, ThreadStep::Phase { phase, .. } if phase == "cleanse_plan")),
            "transition directive must append a phase step"
        );
    }

    #[tokio::test]
    async fn preturn_ladder_stop_fallback_can_be_applied_as_block_directive() {
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let scope = RequestScope {
            tenant: "t".into(),
            workspace: "w".into(),
            project_id: "p".into(),
        };
        let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
        let store = ThreadStore::new(storage, scope, keyspace);
        let tid = "tid-preturn-ladder-stop-fallback";

        let mut st = ExecutionState::new();
        st.hard_mutation_repair_mode = true;
        st.ladder_step = crate::data_engineer::progress_controller::RepairLadderStep::Stop;
        st.last_validate = Some(crate::data_engineer::progress_controller::LastValidateState {
            failed_models: vec![crate::data_engineer::progress_controller::FailedModelRef {
                name: "stg_orders".to_string(),
                file: "models/staging/stg_orders.sql".to_string(),
            }],
            ..Default::default()
        });
        let directive = crate::data_engineer::phase_gate::evaluate_pre_turn_directive(
            &st,
            Phase::CleanseAuthor,
            3,
        );
        let crate::data_engineer::phase_gate::PreTurnDirective::FailFast { kind, reason } = directive
        else {
            panic!("expected failfast directive");
        };
        apply_phase_directive(
            &store,
            tid,
            Some("agent".to_string()),
            Some(Phase::CleanseAuthor),
            PhaseDirective::Block {
                phase: Phase::CleanseAuthor,
                kind,
                reason: reason.clone(),
            },
        )
        .await
        .expect("block directive should append guard");

        let log = store.get(tid).await.expect("thread log should exist");
        assert!(
            log.steps.iter().any(|s| matches!(
                s,
                ThreadStep::GuardBlock { reason: r, .. } if r.contains("stg_orders.sql")
            )),
            "expected guard block with failed-model fallback target in reason"
        );
    }
}
