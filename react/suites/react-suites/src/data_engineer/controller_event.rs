use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValidateFailureClass {
    WarehouseConfig,
    SqlOrRuntime,
    Unknown,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ControllerEvent {
    ValidatePassed,
    ValidateFailed {
        class: ValidateFailureClass,
        signature: FailureSignature,
        brief: String,
        failing_targets: Vec<ValidateFailingTarget>,
        compile_ok: bool,
        run_ok: bool,
    },
    ValidateContractError {
        reason: String,
        brief: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidateFailingTarget {
    pub node_id: String,
    pub canonical_path: String,
    pub error_code: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailureSignature {
    pub class: ValidateFailureClass,
    pub node_id: String,
    pub canonical_path: String,
    pub error_code: String,
}

fn parse_failure_class(v: &Value) -> ValidateFailureClass {
    match v
        .get("class")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .trim()
    {
        "warehouse_config" => ValidateFailureClass::WarehouseConfig,
        "sql_or_runtime" => ValidateFailureClass::SqlOrRuntime,
        _ => ValidateFailureClass::Unknown,
    }
}

fn parse_failing_targets(obs: &Value) -> Vec<ValidateFailingTarget> {
    let Some(arr) = obs.get("failing_targets").and_then(|v| v.as_array()) else {
        return vec![];
    };
    let mut out = Vec::new();
    for it in arr {
        let Some(obj) = it.as_object() else {
            continue;
        };
        let canonical_path = obj
            .get("canonical_path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if canonical_path.is_empty() {
            continue;
        }
        let node_id = obj
            .get("node_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let error_code = obj
            .get("error_code")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if node_id.is_empty() || error_code.is_empty() {
            continue;
        }
        out.push(ValidateFailingTarget {
            node_id,
            canonical_path,
            error_code,
        });
    }
    out
}

pub fn validate_event_from_observation(obs: &Value) -> ControllerEvent {
    let Some(v2) = obs.get("validate_outcome_v2") else {
        return ControllerEvent::ValidateContractError {
            reason: "validate_outcome_v2_missing".to_string(),
            brief: String::new(),
        };
    };
    let ok = v2.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    let compile_ok = v2.get("compile_ok").and_then(|v| v.as_bool()).unwrap_or(false);
    let run_ok = v2.get("run_ok").and_then(|v| v.as_bool()).unwrap_or(false);
    if ok && compile_ok && run_ok {
        return ControllerEvent::ValidatePassed;
    }
    let errs: Vec<String> = obs
        .get("errors")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let brief = crate::data_engineer::dbt_error::compact_brief(&errs, 6, 1200);
    let Some(sig_obj) = v2.get("failure_signature") else {
        return ControllerEvent::ValidateContractError {
            reason: "validate_outcome_v2_missing_failure_signature".to_string(),
            brief,
        };
    };
    let class = parse_failure_class(sig_obj);
    let failing_targets = parse_failing_targets(v2);
    if failing_targets.is_empty() {
        return ControllerEvent::ValidateContractError {
            reason: "validate_outcome_v2_missing_failing_targets".to_string(),
            brief,
        };
    }
    let Some(sig) = sig_obj.as_object() else {
        return ControllerEvent::ValidateContractError {
            reason: "validate_outcome_v2_failure_signature_not_object".to_string(),
            brief,
        };
    };
    let node_id = sig
        .get("node_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let canonical_path = sig
        .get("canonical_path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let error_code = sig
        .get("error_code")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if node_id.is_empty() || canonical_path.is_empty() || error_code.is_empty() {
        return ControllerEvent::ValidateContractError {
            reason: "validate_outcome_v2_failure_signature_incomplete".to_string(),
            brief,
        };
    }
    let signature = FailureSignature {
        class,
        node_id,
        canonical_path,
        error_code,
    };
    ControllerEvent::ValidateFailed {
        class,
        signature,
        brief,
        failing_targets,
        compile_ok,
        run_ok,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_event_classifies_pass() {
        let obs = serde_json::json!({
            "validate_outcome_v2": {
                "ok": true,
                "compile_ok": true,
                "run_ok": true,
                "failing_targets": []
            }
        });
        assert_eq!(
            validate_event_from_observation(&obs),
            ControllerEvent::ValidatePassed
        );
    }

    #[test]
    fn validate_event_classifies_warehouse_failure() {
        let obs = serde_json::json!({
            "errors": ["AccessDenied: athena workgroup not found"],
            "validate_outcome_v2": {
                "ok": false,
                "compile_ok": false,
                "run_ok": false,
                "failure_signature": {
                    "class": "warehouse_config",
                    "node_id": "model.pkg.events",
                    "canonical_path": "models/marts/events.sql",
                    "error_code": "E_ACCESS_DENIED"
                },
                "failing_targets": [{
                    "node_id": "model.pkg.events",
                    "canonical_path": "models/marts/events.sql",
                    "error_code": "E_ACCESS_DENIED"
                }]
            }
        });
        match validate_event_from_observation(&obs) {
            ControllerEvent::ValidateFailed { class, .. } => {
                assert_eq!(class, ValidateFailureClass::WarehouseConfig)
            }
            other => panic!("expected ValidateFailed, got {:?}", other),
        }
    }

    #[test]
    fn validate_event_fails_closed_without_failing_targets() {
        let obs = serde_json::json!({
            "errors": ["Compilation Error"],
            "validate_outcome_v2": {
                "ok": false,
                "compile_ok": false,
                "run_ok": false,
                "failure_signature": {
                    "class": "sql_or_runtime",
                    "node_id": "model.pkg.events",
                    "canonical_path": "models/marts/events.sql",
                    "error_code": "E_SQL"
                },
                "failing_targets": []
            }
        });
        match validate_event_from_observation(&obs) {
            ControllerEvent::ValidateContractError { reason, .. } => {
                assert_eq!(reason, "validate_outcome_v2_missing_failing_targets");
            }
            other => panic!("expected ValidateContractError, got {:?}", other),
        }
    }

    #[test]
    fn validate_event_fails_closed_without_failure_signature() {
        let obs = serde_json::json!({
            "errors": ["Compilation Error"],
            "validate_outcome_v2": {
                "ok": false,
                "compile_ok": false,
                "run_ok": false,
                "failing_targets": [{
                    "node_id": "model.pkg.events",
                    "canonical_path": "models/marts/events.sql",
                    "error_code": "E_SQL"
                }]
            }
        });
        match validate_event_from_observation(&obs) {
            ControllerEvent::ValidateContractError { reason, .. } => {
                assert_eq!(reason, "validate_outcome_v2_missing_failure_signature");
            }
            other => panic!("expected ValidateContractError, got {:?}", other),
        }
    }
}
