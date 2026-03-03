use crate::data_engineer::control_flow::Phase;
use crate::data_engineer::phase_gate;
use crate::data_engineer::progress_controller::{ExecutionState, FailedModelRef};
use crate::data_engineer::state_manager;
use react_core::session::ThreadStore;

pub async fn set_pending_patch_plan_intent(
    thread_store: &ThreadStore,
    thread_id: &str,
    phase: Phase,
    entry_plan_key: Option<String>,
    entry_plan_digest: Option<String>,
) -> Result<(), String> {
    state_manager::mutate_execution_state(thread_store, thread_id, |es| {
        es.set_pending_patch_plan_intent(phase, entry_plan_key.clone(), entry_plan_digest.clone());
    })
    .await
    .map(|_| ())
}

pub async fn set_pending_patch_impl_intent(
    thread_store: &ThreadStore,
    thread_id: &str,
    phase: Phase,
) -> Result<(), String> {
    state_manager::mutate_execution_state(thread_store, thread_id, |es| {
        es.set_pending_patch_impl_intent(phase);
    })
    .await
    .map(|_| ())
}

pub async fn clear_pending_loopback_intent(
    thread_store: &ThreadStore,
    thread_id: &str,
) -> Result<(), String> {
    state_manager::mutate_execution_state(thread_store, thread_id, |es| {
        es.clear_pending_loopback_intent();
    })
    .await
    .map(|_| ())
}

pub fn patch_plan_intent_blocks_fast_forward(
    execution_state: &ExecutionState,
    phase: Phase,
    current_plan_key: &str,
    current_plan_digest: Option<&str>,
) -> bool {
    phase_gate::patch_plan_intent_blocks_fast_forward(
        execution_state,
        phase,
        current_plan_key,
        current_plan_digest,
    )
}

pub fn patch_impl_intent_unsatisfied(execution_state: &ExecutionState, phase: Phase) -> bool {
    phase_gate::patch_impl_intent_unsatisfied(execution_state, phase)
}

pub fn derive_single_target_repair_path(
    execution_state: &ExecutionState,
    last_validate_failed_models: &[FailedModelRef],
) -> Option<String> {
    phase_gate::derive_single_target_repair_path(execution_state, last_validate_failed_models)
}
