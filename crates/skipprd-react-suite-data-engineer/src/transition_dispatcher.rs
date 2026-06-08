use crate::domain_types::GuardBlockKind;
use crate::progress_controller::PhaseTransition;
use crate::state_manager::StateError;
use react_core::session::{Observation, ThreadStep, ThreadStore};
use std::time::Duration;

use crate::control_flow::{
    allowed_next_phases, is_replan_backtrack, replan_backtrack_counter_cap, Phase, TransitionIntent,
};
use crate::progress_controller::ExecutionState;
use crate::state_manager;

pub type PhaseDirective = react_core::workflow::PhaseDirective<Phase, String, GuardBlockKind>;

#[derive(Debug, thiserror::Error)]
pub enum TransitionError {
    #[error("invalid phase transition: from='{from}' to='{to}' reason='{reason}'")]
    InvalidTransition {
        from: String,
        to: String,
        reason: String,
    },
    #[error("state persist failed during transition: {0}")]
    StatePersistFailed(#[from] StateError),
    #[error("failed to append thread step: {0}")]
    AppendStepFailed(String),
}

const PHASE_STEP_APPEND_RETRY_DELAYS_MS: [u64; 3] = [0, 25, 75];

async fn append_phase_step_best_effort(
    store: &ThreadStore,
    thread_id: &str,
    from_phase: Option<Phase>,
    phase: Phase,
    step: ThreadStep,
) {
    let mut last_error: Option<String> = None;
    for (attempt_idx, delay_ms) in PHASE_STEP_APPEND_RETRY_DELAYS_MS.iter().enumerate() {
        if attempt_idx > 0 {
            tokio::time::sleep(Duration::from_millis(*delay_ms)).await;
        }
        match store.append_step(thread_id, step.clone()).await {
            Ok(()) => return,
            Err(e) => {
                let err = e.to_string();
                if attempt_idx + 1 < PHASE_STEP_APPEND_RETRY_DELAYS_MS.len() {
                    tracing::warn!(
                        thread_id = %thread_id,
                        from_phase = ?from_phase.map(|p| p.as_str().to_string()),
                        phase = %phase.as_str(),
                        attempt = attempt_idx + 1,
                        max_attempts = PHASE_STEP_APPEND_RETRY_DELAYS_MS.len(),
                        error = %err,
                        "dispatch_phase_transition: failed to append phase step; retrying"
                    );
                }
                last_error = Some(err);
            }
        }
    }
    if let Some(err) = last_error {
        tracing::warn!(
            thread_id = %thread_id,
            from_phase = ?from_phase.map(|p| p.as_str().to_string()),
            phase = %phase.as_str(),
            attempts = PHASE_STEP_APPEND_RETRY_DELAYS_MS.len(),
            error = %err,
            "dispatch_phase_transition: failed to append phase step after persisting authoritative state"
        );
    }
}

pub async fn dispatch_phase_transition(
    store: &ThreadStore,
    thread_id: &str,
    agent: Option<String>,
    from_phase: Option<Phase>,
    phase: Phase,
    intent: TransitionIntent,
    transition: Option<PhaseTransition>,
) -> Result<(), TransitionError> {
    let reason_str = transition
        .as_ref()
        .map(|t| t.as_reason_str().to_string())
        .unwrap_or_else(|| "none".to_string());

    if let Some(from) = from_phase {
        let is_same_phase_annotation = intent == TransitionIntent::Annotation;
        if !is_same_phase_annotation && !allowed_next_phases(from).contains(&phase) {
            return Err(TransitionError::InvalidTransition {
                from: from.as_str().to_string(),
                to: phase.as_str().to_string(),
                reason: reason_str.clone(),
            });
        }
    }

    let mut st = state_manager::load_execution_state_strict(&store.control_store(), thread_id)
        .await?
        .unwrap_or_else(ExecutionState::new);
    if let Some(from) = from_phase {
        let is_backtrack = is_replan_backtrack(from, phase);
        match intent {
            TransitionIntent::Annotation => {}
            TransitionIntent::Forward => {
                st.with_phase_state_mut(|phase_state| {
                    phase_state.replan_backtracks = 0;
                });
            }
            TransitionIntent::Loopback => {
                let current = st.phase_state().replan_backtracks;
                st.with_phase_state_mut(|phase_state| {
                    phase_state.replan_backtracks = react_core::workflow::next_replan_backtracks(
                        current,
                        intent,
                        is_backtrack,
                        replan_backtrack_counter_cap(),
                    );
                });
            }
        }
    }
    if phase == Phase::ModelPlan && from_phase != Some(Phase::ModelPlan) {
        st.reset_manifest_lookup_state();
    }
    if matches!(phase, Phase::CleansePlan | Phase::ModelPlan) && from_phase != Some(phase) {
        st.reset_plan_bootstrap(phase);
    }
    st.with_phase_state_mut(|phase_state| {
        phase_state.current_phase = phase;
        phase_state.transition = if phase == Phase::Preflight {
            None
        } else {
            transition.clone()
        };
    });
    state_manager::replace_execution_state(&store.control_store(), thread_id, st).await?;

    let reason_detail = transition
        .as_ref()
        .and_then(|t| serde_json::to_value(t).ok());
    let agent = agent.unwrap_or_else(|| crate::env_util::DEFAULT_AGENT_NAME.to_string());
    append_phase_step_best_effort(
        store,
        thread_id,
        from_phase,
        phase,
        ThreadStep::Phase {
            phase: phase.as_str().to_string(),
            from_phase: from_phase.map(|p| p.as_str().to_string()),
            reason_code: Some(reason_str),
            reason_detail,
            observation: Observation::ok(),
            ts: chrono::Utc::now().to_rfc3339(),
            agent,
        },
    )
    .await;

    Ok(())
}

pub async fn apply_phase_directive(
    store: &ThreadStore,
    thread_id: &str,
    agent: Option<String>,
    from_phase: Option<Phase>,
    directive: PhaseDirective,
) -> Result<(), TransitionError> {
    match directive {
        PhaseDirective::Transition {
            to,
            intent,
            reason_code: _,
            reason_detail: _,
        } => dispatch_phase_transition(store, thread_id, agent, from_phase, to, intent, None).await,
        PhaseDirective::Block {
            phase,
            kind,
            reason,
        } => store
            .append_step(
                thread_id,
                ThreadStep::GuardBlock {
                    phase: phase.as_str().to_string(),
                    kind: kind.as_str().to_string(),
                    reason: reason.clone(),
                    observation: Observation::fail(vec![reason]),
                    ts: chrono::Utc::now().to_rfc3339(),
                    agent: agent.unwrap_or_else(|| crate::env_util::DEFAULT_AGENT_NAME.to_string()),
                },
            )
            .await
            .map_err(|e| TransitionError::AppendStepFailed(e.to_string())),
    }
}

// TODO(item-102): Some guardrail tests below overlap with tests_mod.rs (e.g. source-scanning
// tests like `all_phase_executors_use_phase_contract_transition_seam` and
// `run_agent_source_enforces_kernel_transition_and_guard_paths`). Consolidate into a single
// location to avoid drift.
#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use react_core::keyspace::{DefaultKeyspace, Keyspace};
    use react_core::scope::RequestScope;
    use react_core::storage::{ConditionalWriteStatus, StorageAdapter};
    use react_core::CoreError;
    use react_module_storage_memory::InMemoryStorageAdapter;
    use serde_json::Value;
    use std::sync::Arc;
    use std::sync::Mutex;

    #[derive(Clone)]
    struct SelectiveFailStorage {
        inner: Arc<InMemoryStorageAdapter>,
        fail_key: String,
        remaining_failures: Arc<Mutex<Option<usize>>>,
        attempts: Arc<Mutex<usize>>,
    }

    impl SelectiveFailStorage {
        fn new(fail_key: String, remaining_failures: usize) -> Self {
            Self {
                inner: Arc::new(InMemoryStorageAdapter::default()),
                fail_key,
                remaining_failures: Arc::new(Mutex::new(Some(remaining_failures))),
                attempts: Arc::new(Mutex::new(0)),
            }
        }

        fn fail_forever(fail_key: String) -> Self {
            Self {
                inner: Arc::new(InMemoryStorageAdapter::default()),
                fail_key,
                remaining_failures: Arc::new(Mutex::new(None)),
                attempts: Arc::new(Mutex::new(0)),
            }
        }

        fn attempts(&self) -> usize {
            *self.attempts.lock().expect("attempt mutex poisoned")
        }
    }

    #[async_trait]
    impl StorageAdapter for SelectiveFailStorage {
        async fn get_json(&self, key: &str) -> Result<Value, CoreError> {
            self.inner.get_json(key).await
        }

        async fn put_json(&self, key: &str, value: &Value) -> Result<(), CoreError> {
            self.inner.put_json(key, value).await
        }

        async fn put_json_if_etag_matches(
            &self,
            key: &str,
            value: &Value,
            expected_etag: Option<&str>,
        ) -> Result<ConditionalWriteStatus, CoreError> {
            if key == self.fail_key {
                *self.attempts.lock().expect("attempt mutex poisoned") += 1;
                let mut remaining = self
                    .remaining_failures
                    .lock()
                    .expect("remaining_failures mutex poisoned");
                match *remaining {
                    None => {
                        return Err(CoreError::Storage(format!(
                            "synthetic conditional write failure for '{key}'"
                        )));
                    }
                    Some(0) => {}
                    Some(count) => {
                        *remaining = Some(count - 1);
                        return Err(CoreError::Storage(format!(
                            "synthetic conditional write failure for '{key}'"
                        )));
                    }
                }
            }
            self.inner
                .put_json_if_etag_matches(key, value, expected_etag)
                .await
        }

        async fn get_bytes(&self, key: &str) -> Result<Vec<u8>, CoreError> {
            self.inner.get_bytes(key).await
        }

        async fn put_bytes(
            &self,
            key: &str,
            bytes: &[u8],
            content_type: &str,
        ) -> Result<(), CoreError> {
            self.inner.put_bytes(key, bytes, content_type).await
        }

        async fn delete_object(&self, key: &str) -> Result<(), CoreError> {
            self.inner.delete_object(key).await
        }

        async fn head_etag(&self, key: &str) -> Result<Option<String>, CoreError> {
            self.inner.head_etag(key).await
        }

        async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, CoreError> {
            self.inner.list_prefix(prefix).await
        }
    }

    #[tokio::test]
    async fn validate_pass_to_review_resets_counter() {
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let scope = RequestScope::parse("t", "w", "p").expect("valid test scope");
        let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
        let store = ThreadStore::new(storage, scope, keyspace);
        let tid = "tid-validate-pass-reset";

        let mut st = ExecutionState::new();
        st.phase.replan_backtracks = 2;
        state_manager::replace_execution_state(&store.control_store(), tid, st)
            .await
            .expect("seed execution state");

        dispatch_phase_transition(
            &store,
            tid,
            Some("agent".to_string()),
            Some(Phase::CleanseValidate),
            Phase::CleanseReview,
            TransitionIntent::Forward,
            Some(PhaseTransition::ValidatePassToReview { step_idx: 0 }),
        )
        .await
        .expect("transition should succeed");

        let got = state_manager::load_execution_state(&store.control_store(), tid)
            .await
            .expect("state should load")
            .expect("state should exist");
        assert_eq!(
            got.phase.replan_backtracks, 0,
            "forward transitions must reset loopback counter"
        );
    }

    #[tokio::test]
    async fn validate_pass_to_author_increments_loopback_counter() {
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let scope = RequestScope::parse("t", "w", "p").expect("valid test scope");
        let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
        let store = ThreadStore::new(storage, scope, keyspace);
        let tid = "tid-validate-pass-loopback";

        let mut st = ExecutionState::new();
        st.phase.replan_backtracks = 0;
        state_manager::replace_execution_state(&store.control_store(), tid, st)
            .await
            .expect("seed execution state");

        dispatch_phase_transition(
            &store,
            tid,
            Some("agent".to_string()),
            Some(Phase::CleanseValidate),
            Phase::CleanseAuthor,
            TransitionIntent::Loopback,
            Some(PhaseTransition::ValidatePassToAuthoring {
                signal: "test".to_string(),
                plan_key: None,
                pending_count: 0,
            }),
        )
        .await
        .expect("transition should succeed");

        let got = state_manager::load_execution_state(&store.control_store(), tid)
            .await
            .expect("state should load")
            .expect("state should exist");
        assert_eq!(got.phase.replan_backtracks, 1);
    }

    #[tokio::test]
    async fn entering_model_plan_resets_manifest_and_bootstrap_state() {
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let scope = RequestScope::parse("t", "w", "p").expect("valid test scope");
        let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
        let store = ThreadStore::new(storage, scope, keyspace);
        let tid = "tid-model-plan-reset-state";

        let mut st = ExecutionState::new();
        st.manifest.model_plan_bootstrapped = true;
        st.manifest.manifest_lookup.retry_suppressed = true;
        st.manifest.manifest_lookup.repeated_failure_count = 3;
        st.manifest.manifest_lookup.failure_signature = Some("NoSuchKey:Ambiguous".to_string());
        state_manager::replace_execution_state(&store.control_store(), tid, st)
            .await
            .expect("seed execution state");

        dispatch_phase_transition(
            &store,
            tid,
            Some("agent".to_string()),
            Some(Phase::ModelReview),
            Phase::ModelPlan,
            TransitionIntent::Loopback,
            Some(PhaseTransition::ReviewPatchImpl {
                meta: crate::domain_types::ReviewDecisionMeta {
                    decision: crate::domain_types::ReviewDecision::PatchImpl,
                    target_task_ids: vec![],
                    tier: crate::domain_types::ReviewTier::Gold,
                    review_ref: None,
                },
                target_task_ids: vec![],
            }),
        )
        .await
        .expect("transition should succeed");

        let got = state_manager::load_execution_state(&store.control_store(), tid)
            .await
            .expect("state should load")
            .expect("state should exist");
        assert!(
            !got.manifest.model_plan_bootstrapped,
            "model-plan bootstrap should reset on fresh model_plan entry"
        );
        assert!(
            !got.manifest.manifest_lookup.retry_suppressed
                && got.manifest.manifest_lookup.repeated_failure_count == 0,
            "manifest lookup retry state should reset on fresh model_plan entry"
        );
    }

    #[tokio::test]
    async fn apply_phase_directive_block_appends_guard() {
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let scope = RequestScope::parse("t", "w", "p").expect("valid test scope");
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
            steps.iter().any(
                |s| matches!(s, ThreadStep::GuardBlock { phase, .. } if phase == "cleanse_author")
            ),
            "guard block must be appended by directive applier"
        );
    }

    #[tokio::test]
    async fn apply_phase_directive_annotate_keeps_backtrack_counter() {
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let scope = RequestScope::parse("t", "w", "p").expect("valid test scope");
        let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
        let store = ThreadStore::new(storage, scope, keyspace);
        let tid = "tid-phase-directive-annotate";
        let mut st = ExecutionState::new();
        st.phase.current_phase = Phase::CleansePlan;
        st.phase.replan_backtracks = 2;
        state_manager::replace_execution_state(&store.control_store(), tid, st)
            .await
            .expect("seed state");

        apply_phase_directive(
            &store,
            tid,
            Some("agent".to_string()),
            Some(Phase::CleansePlan),
            PhaseDirective::Transition {
                to: Phase::CleansePlan,
                intent: TransitionIntent::Annotation,
                reason_code: Some("phase_set".to_string()),
                reason_detail: Some(serde_json::json!({"note":"x"})),
            },
        )
        .await
        .expect("annotation should succeed");

        let got = state_manager::load_execution_state(&store.control_store(), tid)
            .await
            .expect("state")
            .expect("state should exist");
        assert_eq!(got.phase.replan_backtracks, 2);
    }

    #[tokio::test]
    async fn loopback_counter_saturates_at_cap() {
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let scope = RequestScope::parse("t", "w", "p").expect("valid test scope");
        let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
        let store = ThreadStore::new(storage, scope, keyspace);
        let tid = "tid-loopback-cap";
        let mut st = ExecutionState::new();
        st.phase.current_phase = Phase::CleanseValidate;
        st.phase.replan_backtracks = replan_backtrack_counter_cap();
        state_manager::replace_execution_state(&store.control_store(), tid, st)
            .await
            .expect("seed state");

        dispatch_phase_transition(
            &store,
            tid,
            Some("agent".to_string()),
            Some(Phase::CleanseValidate),
            Phase::CleanseAuthor,
            TransitionIntent::Loopback,
            Some(PhaseTransition::ValidatePassToAuthoring {
                signal: "test".to_string(),
                plan_key: None,
                pending_count: 0,
            }),
        )
        .await
        .expect("transition should succeed");

        let got = state_manager::load_execution_state(&store.control_store(), tid)
            .await
            .expect("state")
            .expect("state should exist");
        assert_eq!(got.phase.replan_backtracks, replan_backtrack_counter_cap());
    }

    #[tokio::test]
    async fn apply_phase_directive_transition_is_handled() {
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let scope = RequestScope::parse("t", "w", "p").expect("valid test scope");
        let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
        let store = ThreadStore::new(storage, scope, keyspace);
        let tid = "tid-phase-directive-transition";

        apply_phase_directive(
            &store,
            tid,
            Some("agent".to_string()),
            Some(Phase::Preflight),
            PhaseDirective::Transition {
                to: Phase::CleansePlan,
                intent: TransitionIntent::Forward,
                reason_code: Some("preflight_ok".to_string()),
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
    async fn publish_confirmed_failure_can_return_to_model_authoring() {
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let scope = RequestScope::parse("t", "w", "p").expect("valid test scope");
        let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
        let store = ThreadStore::new(storage, scope, keyspace);
        let tid = "tid-publish-confirmed-fail";

        dispatch_phase_transition(
            &store,
            tid,
            Some("agent".to_string()),
            Some(Phase::Publish),
            Phase::ModelAuthor,
            TransitionIntent::Loopback,
            Some(PhaseTransition::PublishConfirmedFail),
        )
        .await
        .expect("publish failure repair loopback should be valid");

        let got = state_manager::load_execution_state(&store.control_store(), tid)
            .await
            .expect("state should load")
            .expect("state should exist");
        assert_eq!(got.phase.current_phase, Phase::ModelAuthor);
    }

    #[tokio::test]
    async fn dispatch_phase_transition_retries_phase_step_append_until_success() {
        let scope = RequestScope::parse("t", "w", "p").expect("valid test scope");
        let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
        let tid = "tid-phase-step-retry";
        let fail_key = keyspace
            .thread_key(&scope, tid)
            .expect("thread key should build");
        let storage = Arc::new(SelectiveFailStorage::new(
            fail_key,
            PHASE_STEP_APPEND_RETRY_DELAYS_MS.len() - 1,
        ));
        let store = ThreadStore::new(storage.clone(), scope, keyspace);

        dispatch_phase_transition(
            &store,
            tid,
            Some("agent".to_string()),
            Some(Phase::Preflight),
            Phase::CleansePlan,
            TransitionIntent::Forward,
            Some(PhaseTransition::PreflightOk {
                dbt_project_key: "dbt/project.yml".to_string(),
                has_query_provider: true,
                has_dbt_provider: true,
            }),
        )
        .await
        .expect("transition should succeed after bounded retries");

        assert_eq!(storage.attempts(), PHASE_STEP_APPEND_RETRY_DELAYS_MS.len());
        let got = state_manager::load_execution_state(&store.control_store(), tid)
            .await
            .expect("state should load")
            .expect("state should exist");
        assert_eq!(got.phase.current_phase, Phase::CleansePlan);

        let log = store.get(tid).await.expect("thread log should exist");
        assert!(
            log.steps
                .iter()
                .any(|s| matches!(s, ThreadStep::Phase { phase, .. } if phase == "cleanse_plan")),
            "phase step should eventually be appended after retrying"
        );
    }

    #[tokio::test]
    async fn dispatch_phase_transition_keeps_state_when_phase_step_append_never_recovers() {
        let scope = RequestScope::parse("t", "w", "p").expect("valid test scope");
        let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
        let tid = "tid-phase-step-persisted-state";
        let fail_key = keyspace
            .thread_key(&scope, tid)
            .expect("thread key should build");
        let storage = Arc::new(SelectiveFailStorage::fail_forever(fail_key));
        let store = ThreadStore::new(storage.clone(), scope, keyspace);

        dispatch_phase_transition(
            &store,
            tid,
            Some("agent".to_string()),
            Some(Phase::Preflight),
            Phase::CleansePlan,
            TransitionIntent::Forward,
            Some(PhaseTransition::PreflightOk {
                dbt_project_key: "dbt/project.yml".to_string(),
                has_query_provider: true,
                has_dbt_provider: true,
            }),
        )
        .await
        .expect("authoritative transition state should not roll back");

        assert!(
            storage.attempts() >= PHASE_STEP_APPEND_RETRY_DELAYS_MS.len(),
            "phase step append should exhaust the configured retry budget"
        );
        let got = state_manager::load_execution_state(&store.control_store(), tid)
            .await
            .expect("state should load")
            .expect("state should exist");
        assert_eq!(got.phase.current_phase, Phase::CleansePlan);

        let log = store.get(tid).await;
        assert!(
            log.is_err()
                || !log.expect("checked is_err above").steps.iter().any(
                    |s| matches!(s, ThreadStep::Phase { phase, .. } if phase == "cleanse_plan")
                ),
            "phase transition should stay authoritative even when the thread phase step is lost"
        );
    }

    // -----------------------------------------------------------------------
    // Architectural guardrail tests
    //
    // These tests use `include_str!` to statically inspect sibling source
    // files and assert structural invariants (e.g. "phase_review must use
    // typed reason_detail constructors, not inline struct literals").
    //
    // They are co-located here because they guard the transition dispatcher's
    // callers and are run as part of the normal `cargo test` suite. Moving
    // them to a separate crate-level integration module would lose the
    // locality benefit without a meaningful architectural win.
    // -----------------------------------------------------------------------

    #[test]
    fn control_review_and_publish_use_phase_transition_variants() {
        let phase_review_src = include_str!("phase_review.rs");
        assert!(
            phase_review_src.contains("PhaseTransition::ReviewProceed")
                || phase_review_src.contains("PhaseTransition::ReviewPatchImpl"),
            "review transitions must use typed PhaseTransition variants"
        );

        let phase_publish_src = include_str!("phase_publish.rs");
        for marker in [
            "PhaseTransition::PublishApproved",
            "PhaseTransition::PublishConfirmedSuccess",
            "PhaseTransition::PublishFail",
            "PhaseTransition::PublishConfirmedFail",
        ] {
            assert!(
                phase_publish_src.contains(marker),
                "phase_publish missing PhaseTransition variant: {marker}"
            );
        }
    }

    #[test]
    fn control_critical_publish_uses_phase_transition_variants() {
        let src = include_str!("phase_publish.rs");
        assert!(
            src.contains("PhaseTransition::PublishApproved"),
            "publish approval transition must use PhaseTransition::PublishApproved"
        );
        assert!(
            src.contains("PhaseTransition::PublishConfirmedSuccess"),
            "publish success transitions must use PhaseTransition::PublishConfirmedSuccess"
        );
        assert!(
            src.contains("PhaseTransition::PublishFail"),
            "publish failure transitions must use PhaseTransition::PublishFail"
        );
    }

    #[test]
    fn control_critical_author_uses_phase_transition_variants() {
        let src = include_str!("phase_author.rs");
        assert!(
            src.contains("PhaseTransition::PlanMissing"),
            "authoring plan-missing loopback must use PhaseTransition::PlanMissing"
        );
        assert!(
            src.contains("PhaseTransition::PlanNotApproved"),
            "authoring plan-not-approved loopback must use PhaseTransition::PlanNotApproved"
        );
        assert!(
            src.contains("commit_plan_revision_loopback("),
            "authoring semantic-invalid paths must use commit_plan_revision_loopback"
        );
        assert!(
            src.contains("PhaseTransition::AuthoringComplete"),
            "authoring complete must use PhaseTransition::AuthoringComplete"
        );
    }

    #[test]
    fn publish_await_phase_is_side_effect_free() {
        let src = include_str!("phase_publish.rs");
        let Some(await_start) =
            src.find("pub(super) async fn execute_publish_await_approval_phase")
        else {
            panic!("phase_publish missing execute_publish_await_approval_phase");
        };
        let Some(publish_start) = src.find("pub(super) async fn execute_publish_phase") else {
            panic!("phase_publish missing execute_publish_phase");
        };
        let await_section = &src[await_start..publish_start];
        assert!(
            !await_section.contains("call_and_record_tool("),
            "publish_await_approval must not invoke publish side-effect tools"
        );
        assert!(
            await_section.contains("gate_publish_progress"),
            "publish_await_approval must enforce explicit approval gate"
        );
    }

    #[test]
    fn review_patch_impl_routes_through_evaluation() {
        let src = include_str!("phase_review.rs");
        assert!(
            src.contains("Evidence::Review"),
            "phase_review should produce review evidence instead of phase-aware repair routing"
        );
        assert!(
            src.contains("apply_evaluation_verdict"),
            "phase_review should apply central evaluation verdicts"
        );
        assert!(
            !src.contains("patch_impl_target_phase"),
            "patch-impl should no longer route through global repair-mode helpers"
        );
    }

    #[test]
    fn preflight_uses_phase_contract_decision_commit() {
        let src = include_str!("phase_preflight.rs");
        assert!(
            src.contains("commit_phase_decision"),
            "phase_preflight should commit transitions through the shared phase contract helper"
        );
        assert!(
            src.contains("PhaseDecision::forward"),
            "phase_preflight should use shared phase decision constructors"
        );
    }

    #[test]
    fn validate_and_author_use_phase_contract_transition_seam() {
        let validate_src = include_str!("phase_validate.rs");
        assert!(
            validate_src.contains("commit_phase_decision"),
            "phase_validate should commit transitions through phase contract helpers"
        );
        assert!(
            !validate_src.contains("apply_phase_transition("),
            "phase_validate should not bypass commit_phase_decision"
        );
        assert!(
            validate_src.contains("reduce_validate_pass_plan_state"),
            "phase_validate should route validate-pass through one reducer"
        );
        assert!(
            validate_src.contains("reduce_validate_pass_plan_state(&actx, phase)"),
            "phase_validate should call the validate-pass reducer before committing transition"
        );
        assert!(
            validate_src.contains("commit_validate_pass_transition"),
            "phase_validate should commit validate-pass transitions through one helper"
        );
        assert!(
            validate_src.contains(".run_observed("),
            "phase_validate must use ThreadStore::run_observed for tool step logging"
        );
        assert!(
            !validate_src.contains("ThreadStep::ToolStart {"),
            "phase_validate must not append raw ToolStart steps directly; use ThreadStore::run_observed"
        );
        assert!(
            !validate_src.contains("ThreadStep::ToolEnd {"),
            "phase_validate must not append raw ToolEnd steps directly; use ThreadStore::run_observed"
        );
        assert!(
            !validate_src.contains(&format!("{}{}", "thread_store", ".get(thread_id)")),
            "phase_validate must not derive runtime control metadata from thread log replay"
        );

        let author_src = include_str!("phase_author.rs");
        assert!(
            author_src.contains("commit_phase_decision"),
            "phase_author should commit transitions through phase contract helpers"
        );
        assert!(
            !author_src.contains("apply_phase_transition("),
            "phase_author should not bypass commit_phase_decision"
        );
        assert!(
            author_src.contains("decide_author_validate_trigger"),
            "phase_author should use a single author->validate decision helper"
        );

        let review_src = include_str!("phase_review.rs");
        assert!(
            !review_src.contains(&format!("{}{}", "thread_store", ".get(thread_id)")),
            "phase_review must not derive runtime control metadata from thread log replay"
        );
    }

    #[test]
    fn all_phase_executors_use_phase_contract_transition_seam() {
        let plan_src = include_str!("phase_plan.rs");
        let review_src = include_str!("phase_review.rs");
        let publish_src = include_str!("phase_publish.rs");
        let helpers_src = include_str!("plan_review_helpers.rs");

        for (name, src) in [
            ("phase_plan", plan_src),
            ("phase_review", review_src),
            ("phase_publish", publish_src),
            ("plan_review_helpers", helpers_src),
        ] {
            assert!(
                src.contains("commit_phase_decision"),
                "{name} should commit transitions through commit_phase_decision"
            );
            assert!(
                !src.contains("apply_phase_transition("),
                "{name} should not bypass commit_phase_decision"
            );
        }
    }

    #[test]
    fn tool_observability_uses_core_run_observed() {
        let control_flow_src = include_str!("control_flow.rs");
        assert!(
            control_flow_src.contains(".run_observed("),
            "call_and_record_tool must delegate to ThreadStore::run_observed"
        );
        assert!(
            !control_flow_src.contains("ThreadStep::ToolStart {"),
            "control_flow must not construct raw ToolStart; use ThreadStore::run_observed"
        );
        assert!(
            !control_flow_src.contains("ThreadStep::ToolEnd {"),
            "control_flow must not construct raw ToolEnd; use ThreadStore::run_observed"
        );

        let policy_src = include_str!("policy_sql_validated.rs");
        assert!(
            policy_src.contains(".run_observed("),
            "policy_sql_validated must delegate to ThreadStore::run_observed for run_sql"
        );
    }

    #[test]
    fn repair_state_no_longer_uses_status_enum() {
        let src = include_str!("progress_controller.rs");
        assert!(
            !src.contains(&format!("{}{}", "Repair", "Status")),
            "repair status enum should be gone; verdicts and attempt ledger drive loops"
        );
        assert!(
            !src.contains("pub repair_active: bool"),
            "legacy repair_active bool must not exist after status enum migration"
        );
        assert!(
            !src.contains("pub mutation_epoch: u64"),
            "legacy mutation_epoch must not exist after migration"
        );
    }
}
