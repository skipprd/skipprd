use react_core::agent::AgentCtx;
use serde_json::Value;
use std::collections::HashSet;

use crate::data_engineer::progress_controller::{BatchFailureKind, DataEngineerEvent};
use crate::data_engineer::state_manager;

pub(crate) fn classify_batch_failure_kind(errors: &[String]) -> BatchFailureKind {
    let joined = errors.join("\n").to_ascii_lowercase();
    if joined.contains("sql validation failed")
        || joined.contains("column_not_found")
        || joined.contains("compilation error")
        || joined.contains("runtime error")
    {
        return BatchFailureKind::SqlValidation;
    }
    if joined.contains("schema")
        || joined.contains("contract")
        || joined.contains("yaml")
        || joined.contains("parse")
    {
        return BatchFailureKind::SchemaOrContract;
    }
    if joined.contains("service error")
        || joined.contains("timeout")
        || joined.contains("throttle")
        || joined.contains("temporar")
        || joined.contains("http 502")
        || joined.contains("http 503")
        || joined.contains("http 504")
    {
        return BatchFailureKind::InfraTransient;
    }
    BatchFailureKind::Unknown
}

pub(crate) fn classify_schema_batch_failure_kind(msg: &str) -> BatchFailureKind {
    let s = msg.to_ascii_lowercase();
    if s.contains("timeout")
        || s.contains("temporar")
        || s.contains("http 502")
        || s.contains("http 503")
        || s.contains("http 504")
    {
        return BatchFailureKind::InfraTransient;
    }
    if s.contains("schema") || s.contains("yaml") || s.contains("parse") || s.contains("column") {
        return BatchFailureKind::SchemaOrContract;
    }
    BatchFailureKind::Unknown
}

pub(crate) async fn emit_batch_event(ctx: &AgentCtx, event: DataEngineerEvent) -> Result<(), String> {
    let Some(thread_store) = ctx.thread_store.as_ref() else {
        return Ok(());
    };
    let Some(thread_id) = ctx.thread_id.as_deref() else {
        return Ok(());
    };
    state_manager::apply_execution_event(thread_store, thread_id, event)
        .await
        .map(|_| ())
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

pub(crate) fn extract_errors(res: &Value) -> Vec<String> {
    res.get("errors")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

pub(crate) fn extract_first_error(res: &Value, default: &str) -> String {
    res.get("errors")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| default.to_string())
}
