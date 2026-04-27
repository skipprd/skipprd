use react_core::session::ControlStateStore;
use react_core::CoreError;

use crate::progress_controller::{
    DataEngineerEvent, ExecutionState, EXECUTION_STATE_SCHEMA_VERSION,
};

pub const DATA_ENGINEER_SUITE_ID: &str = "data_engineer";

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("state storage failed: {0}")]
    StorageFailed(String),
    #[error("state deserialization/validation failed for '{key}': {detail}")]
    DeserializeFailed { key: String, detail: String },
    #[error("state invariant violated: {0}")]
    ValidationFailed(String),
}

impl From<CoreError> for StateError {
    fn from(e: CoreError) -> Self {
        match e {
            CoreError::Serialization(e) => StateError::DeserializeFailed {
                key: String::new(),
                detail: e.to_string(),
            },
            other => StateError::StorageFailed(other.to_string()),
        }
    }
}

fn validate_loaded_state(parsed: &ExecutionState) -> Result<(), StateError> {
    if parsed.schema_version != EXECUTION_STATE_SCHEMA_VERSION {
        return Err(StateError::DeserializeFailed {
            key: "execution_state".into(),
            detail: format!(
                "schema_version mismatch: expected {}, got {}",
                EXECUTION_STATE_SCHEMA_VERSION, parsed.schema_version
            ),
        });
    }
    parsed
        .validate_invariants()
        .map_err(|e| StateError::ValidationFailed(format!("invariant check failed on load: {e}")))
}

pub async fn load_execution_state(
    control: &ControlStateStore,
    thread_id: &str,
) -> Result<Option<ExecutionState>, StateError> {
    let Some(parsed) = control
        .load::<ExecutionState>(thread_id, DATA_ENGINEER_SUITE_ID)
        .await?
    else {
        return Ok(None);
    };
    validate_loaded_state(&parsed)?;
    Ok(Some(parsed))
}

pub async fn load_execution_state_strict(
    control: &ControlStateStore,
    thread_id: &str,
) -> Result<Option<ExecutionState>, StateError> {
    let loaded = control
        .load::<ExecutionState>(thread_id, DATA_ENGINEER_SUITE_ID)
        .await?;
    let Some(parsed) = loaded else {
        return Ok(None);
    };
    validate_loaded_state(&parsed)?;
    Ok(Some(parsed))
}

async fn mutate_execution_state(
    control: &ControlStateStore,
    thread_id: &str,
    mutate: impl FnOnce(&mut ExecutionState),
) -> Result<ExecutionState, StateError> {
    control
        .mutate::<ExecutionState>(thread_id, DATA_ENGINEER_SUITE_ID, |current| {
            let mut st = current.unwrap_or_else(ExecutionState::new);
            mutate(&mut st);
            st.validate_invariants().map_err(|e| {
                CoreError::Session(format!(
                    "execution_state invariant check failed after mutation: {e}"
                ))
            })?;
            Ok(st)
        })
        .await
        .map_err(StateError::from)
}

pub async fn replace_execution_state(
    control: &ControlStateStore,
    thread_id: &str,
    next_state: ExecutionState,
) -> Result<ExecutionState, StateError> {
    mutate_execution_state(control, thread_id, |st| {
        *st = next_state.clone();
    })
    .await
}

pub async fn apply_execution_event(
    control: &ControlStateStore,
    thread_id: &str,
    event: DataEngineerEvent,
) -> Result<ExecutionState, StateError> {
    mutate_execution_state(control, thread_id, |st| st.apply_event(event)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use react_core::keyspace::DefaultKeyspace;
    use react_core::scope::RequestScope;
    use react_module_storage_memory::InMemoryStorageAdapter;
    use std::sync::Arc;

    fn test_control_store() -> ControlStateStore {
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let scope = RequestScope::parse("t", "w", "p").expect("valid test scope");
        let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
        ControlStateStore::new(storage, scope, keyspace)
    }

    #[tokio::test]
    async fn mutate_execution_state_rejects_invalid_mutator_result() {
        let control = test_control_store();
        let tid = "tid-state-manager-invariant-mutate";
        let err = mutate_execution_state(&control, tid, |st| {
            // Probe active without failure_context → invariant violation
            st.telemetry.probe = crate::progress_controller::ProbeStatus::Required {
                attempts: crate::progress_controller::ProbeAttempts::default(),
            };
        })
        .await
        .expect_err("invalid post-mutation state must fail");
        assert!(
            !err.to_string().trim().is_empty(),
            "error should be non-empty"
        );
    }
}
