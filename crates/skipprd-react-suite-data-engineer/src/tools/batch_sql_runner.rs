use react_core::agent::AgentCtx;
use serde_json::Value;
use std::collections::HashSet;

use crate::failure_kind::FailureKind;
use crate::progress_controller::DataEngineerEvent;
use crate::state_manager;

pub(crate) fn extract_batch_failure_kind(res: &Value) -> Result<FailureKind, String> {
    let raw = res
        .get("batch_failure_kind")
        .cloned()
        .ok_or_else(|| "missing required field batch_failure_kind".to_string())?;
    serde_json::from_value::<FailureKind>(raw)
        .map_err(|e| format!("invalid batch_failure_kind value: {e}"))
}

pub(crate) fn classify_schema_batch_failure_kind(msg: &str) -> FailureKind {
    let s = crate::failure_text::normalize_text(msg);
    if crate::failure_text::matches_infra_transient(&s) {
        return FailureKind::InfraTransient;
    }
    FailureKind::Unknown
}

pub(crate) fn classify_authoring_batch_failure_kind(msg: &str) -> FailureKind {
    let s = crate::failure_text::normalize_text(msg);
    if crate::failure_text::matches_infra_transient(&s) {
        return FailureKind::InfraTransient;
    }
    FailureKind::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_authoring_non_infra_is_unknown() {
        let k = classify_authoring_batch_failure_kind(
            "sql validation failed: Athena/Trino cannot reference a SELECT-list alias",
        );
        assert_eq!(k, FailureKind::Unknown);
    }

    #[test]
    fn classify_authoring_schema_error_is_unknown() {
        let k = classify_authoring_batch_failure_kind("invalid model folder 'models/raw/x.sql'");
        assert_eq!(k, FailureKind::Unknown);
    }

    #[test]
    fn classify_authoring_service_error_is_transient() {
        let k = classify_authoring_batch_failure_kind(
            "AwsDataCatalog.test_raw.raw_customers: sql validation failed: service error",
        );
        assert_eq!(k, FailureKind::InfraTransient);
    }

    #[test]
    fn classify_authoring_throttling_is_transient() {
        let k = classify_authoring_batch_failure_kind("ThrottlingException: rate exceeded");
        assert_eq!(k, FailureKind::InfraTransient);
    }
}

pub(crate) async fn emit_batch_event(
    ctx: &AgentCtx,
    event: DataEngineerEvent,
) -> Result<(), String> {
    let Some(thread_store) = ctx.thread_store().as_ref() else {
        return Ok(());
    };
    let Some(thread_id) = ctx.thread_id().as_deref() else {
        return Ok(());
    };
    state_manager::apply_execution_event(&thread_store.control_store(), thread_id, event)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

pub(crate) fn parse_succeeded_ids(res: &Value, field: &str) -> Vec<String> {
    res.get(field)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn derive_failed_ids(attempted: &[String], succeeded: &[String]) -> Vec<String> {
    let succ_set: HashSet<String> = succeeded.iter().cloned().collect();
    attempted
        .iter()
        .filter(|id| !succ_set.contains(*id))
        .cloned()
        .collect()
}

pub(crate) fn extract_first_error(res: &Value, default: &str) -> String {
    res.get("errors")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| default.to_string())
}
