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
        fingerprint: String,
        brief: String,
        failing_models: Vec<Value>,
        compile_ok: bool,
        run_ok: bool,
    },
}

pub fn validate_event_from_observation(obs: &Value) -> ControllerEvent {
    let ok = obs.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    let compile_ok = obs
        .get("compile_ok")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let run_ok = obs.get("run_ok").and_then(|v| v.as_bool()).unwrap_or(false);
    if ok && compile_ok && run_ok {
        return ControllerEvent::ValidatePassed;
    }

    let errs: Vec<String> = obs
        .get("errors")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let class = match crate::data_engineer::dbt_error::classify(&errs) {
        crate::data_engineer::dbt_error::DbtErrorClass::WarehouseConfig => {
            ValidateFailureClass::WarehouseConfig
        }
        crate::data_engineer::dbt_error::DbtErrorClass::SqlFailure
        | crate::data_engineer::dbt_error::DbtErrorClass::SqlOrModel => {
            ValidateFailureClass::SqlOrRuntime
        }
        _ => ValidateFailureClass::Unknown,
    };
    let brief = crate::data_engineer::dbt_error::compact_brief(&errs, 6, 1200);
    let failing_models = crate::data_engineer::dbt_error::extract_failed_models_from_logs(
        &obs.get("logs").cloned().unwrap_or(Value::Null),
    );
    let fingerprint = crate::data_engineer::failure_classifier::fingerprint_validate_observation(obs);
    ControllerEvent::ValidateFailed {
        class,
        fingerprint,
        brief,
        failing_models,
        compile_ok,
        run_ok,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_event_classifies_pass() {
        let obs = serde_json::json!({"ok":true,"compile_ok":true,"run_ok":true});
        assert_eq!(
            validate_event_from_observation(&obs),
            ControllerEvent::ValidatePassed
        );
    }

    #[test]
    fn validate_event_classifies_warehouse_failure() {
        let obs = serde_json::json!({
            "ok": false,
            "compile_ok": false,
            "run_ok": false,
            "errors": ["AccessDenied: athena workgroup not found"]
        });
        match validate_event_from_observation(&obs) {
            ControllerEvent::ValidateFailed { class, .. } => {
                assert_eq!(class, ValidateFailureClass::WarehouseConfig)
            }
            other => panic!("expected ValidateFailed, got {:?}", other),
        }
    }
}
