use react_core::session::{ThreadState, ThreadStore, THREAD_STATE_SCHEMA_VERSION};

use crate::data_engineer::progress_controller::{
    ExecutionState, EXECUTION_STATE_SCHEMA_VERSION,
};

pub async fn load_execution_state(thread_store: &ThreadStore, thread_id: &str) -> Option<ExecutionState> {
    let st = thread_store.get_thread_state(thread_id).await.ok()?;
    let raw = st.control_state?;
    let parsed = serde_json::from_value::<ExecutionState>(raw).ok()?;
    if parsed.schema_version != EXECUTION_STATE_SCHEMA_VERSION {
        return None;
    }
    Some(parsed)
}

pub async fn load_execution_state_strict(
    thread_store: &ThreadStore,
    thread_id: &str,
) -> Result<Option<ExecutionState>, String> {
    let st = thread_store
        .get_thread_state(thread_id)
        .await
        .map_err(|e| format!("failed to load thread_state for execution_state: {e}"))?;
    let Some(raw) = st.control_state else {
        return Ok(None);
    };
    let parsed = serde_json::from_value::<ExecutionState>(raw)
        .map_err(|e| format!("failed to parse execution_state control_state payload: {e}"))?;
    if parsed.schema_version != EXECUTION_STATE_SCHEMA_VERSION {
        return Err(format!(
            "execution_state schema_version mismatch: expected {}, got {}",
            EXECUTION_STATE_SCHEMA_VERSION, parsed.schema_version
        ));
    }
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
    let mut st = thread_store
        .get_thread_state(thread_id)
        .await
        .unwrap_or_else(|_| ThreadState {
            thread_state_schema_version: THREAD_STATE_SCHEMA_VERSION,
            thread_id: thread_id.to_string(),
            ..ThreadState::default()
        });
    st.control_state = Some(serde_json::to_value(state).map_err(|e| e.to_string())?);
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
