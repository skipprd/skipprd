use react_core::session::{ThreadState, ThreadStore, THREAD_STATE_SCHEMA_VERSION};

use crate::data_engineer::progress_controller::{
    DataEngineerEvent, ExecutionState, EXECUTION_STATE_SCHEMA_VERSION,
};

const DATA_ENGINEER_SUITE_ID: &str = "data_engineer";

fn parse_execution_state(raw: serde_json::Value) -> Result<ExecutionState, String> {
    let parsed = serde_json::from_value::<ExecutionState>(raw)
        .map_err(|e| format!("failed to parse execution_state payload: {e}"))?;
    if parsed.schema_version != EXECUTION_STATE_SCHEMA_VERSION {
        return Err(format!(
            "execution_state schema_version mismatch: expected {}, got {}",
            EXECUTION_STATE_SCHEMA_VERSION, parsed.schema_version
        ));
    }
    Ok(parsed)
}

pub async fn load_execution_state(thread_store: &ThreadStore, thread_id: &str) -> Option<ExecutionState> {
    let raw = thread_store
        .load_control_state_payload(thread_id, DATA_ENGINEER_SUITE_ID)
        .await
        .ok()??;
    parse_execution_state(raw).ok()
}

pub async fn load_execution_state_strict(
    thread_store: &ThreadStore,
    thread_id: &str,
) -> Result<Option<ExecutionState>, String> {
    let Some(raw) = thread_store
        .load_control_state_payload(thread_id, DATA_ENGINEER_SUITE_ID)
        .await
        .map_err(|e| format!("failed to load thread_state for execution_state: {e}"))?
    else {
        return Ok(None);
    };
    let parsed = parse_execution_state(raw)?;
    Ok(Some(parsed))
}

pub async fn save_execution_state(
    thread_store: &ThreadStore,
    thread_id: &str,
    state: &ExecutionState,
) -> Result<(), String> {
    if state.schema_version != EXECUTION_STATE_SCHEMA_VERSION {
        return Err(format!(
            "execution_state schema_version mismatch: expected {}, got {}",
            EXECUTION_STATE_SCHEMA_VERSION, state.schema_version
        ));
    }
    thread_store
        .save_control_state_payload(
            thread_id,
            DATA_ENGINEER_SUITE_ID,
            serde_json::to_value(state).map_err(|e| e.to_string())?,
        )
        .await?;
    // Keep existing mirrored phase summary behavior.
    let mut st = ThreadState {
        thread_state_schema_version: THREAD_STATE_SCHEMA_VERSION,
        thread_id: thread_id.to_string(),
        ..ThreadState::default()
    };
    st.current_phase = state.current_phase.as_ref().map(|p| p.as_str().to_string());
    thread_store.put_thread_state(thread_id, &st).await
}

pub async fn mutate_execution_state(
    thread_store: &ThreadStore,
    thread_id: &str,
    mutate: impl FnOnce(&mut ExecutionState),
) -> Result<ExecutionState, String> {
    let mut st = load_execution_state_strict(thread_store, thread_id)
        .await?
        .unwrap_or_else(ExecutionState::new);
    mutate(&mut st);
    save_execution_state(thread_store, thread_id, &st).await?;
    Ok(st)
}

pub async fn apply_execution_event(
    thread_store: &ThreadStore,
    thread_id: &str,
    event: DataEngineerEvent,
) -> Result<ExecutionState, String> {
    mutate_execution_state(thread_store, thread_id, |st| st.apply_event(event))
        .await
}
