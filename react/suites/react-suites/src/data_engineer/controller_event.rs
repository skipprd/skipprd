use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type ValidateTargetPath = crate::data_engineer::progress_controller::RepairTargetPath;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidateFailingTarget {
    pub node_id: String,
    #[serde(rename = "canonical_path")]
    pub target_path: ValidateTargetPath,
    pub error_code: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureSignature {
    pub class: ValidateFailureClass,
    pub node_id: String,
    #[serde(rename = "canonical_path")]
    pub target_path: ValidateTargetPath,
    pub error_code: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidateOutcomeV2 {
    pub ok: bool,
    pub compile_ok: bool,
    pub run_ok: bool,
    #[serde(default)]
    pub failing_targets: Vec<ValidateFailingTarget>,
    #[serde(default)]
    pub failure_signature: Option<FailureSignature>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ValidateObservationContract {
    pub observation: Value,
    pub outcome_v2: ValidateOutcomeV2,
}

impl ValidateObservationContract {
    pub fn into_observation(self) -> Value {
        self.observation
    }
}

fn failure_class_from_validate_result(obj: &serde_json::Map<String, Value>) -> ValidateFailureClass {
    let class = obj
        .get("failure_class")
        .cloned()
        .and_then(|v| serde_json::from_value::<react_core::providers::DbtFailureClass>(v).ok())
        .unwrap_or(react_core::providers::DbtFailureClass::Unknown);
    match class {
        react_core::providers::DbtFailureClass::WarehouseConfig => {
            ValidateFailureClass::WarehouseConfig
        }
        react_core::providers::DbtFailureClass::SqlOrRuntime => ValidateFailureClass::SqlOrRuntime,
        _ => ValidateFailureClass::Unknown,
    }
}

fn failure_class_key(class: ValidateFailureClass) -> &'static str {
    match class {
        ValidateFailureClass::WarehouseConfig => "warehouse_config",
        ValidateFailureClass::SqlOrRuntime => "sql_or_runtime",
        ValidateFailureClass::Unknown => "unknown",
    }
}

fn extract_models_path_hint(raw_line: &str) -> Option<String> {
    let idx = raw_line.find("models/")?;
    let tail = &raw_line[idx..];
    let candidate = tail
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != ')' && *c != ',' && *c != ';')
        .collect::<String>()
        .trim_matches('"')
        .trim_matches('\'')
        .to_string();
    if candidate.ends_with(".sql") || candidate.ends_with(".yml") || candidate.ends_with(".yaml") {
        return Some(candidate);
    }
    None
}

fn node_hint_from_line(raw_line: &str) -> Option<String> {
    for marker in ["Failure in model ", "Runtime Error in model ", " model "] {
        if let Some((_, rhs)) = raw_line.split_once(marker) {
            let token = rhs.split_whitespace().next().unwrap_or("").trim();
            if !token.is_empty() {
                let node = token
                    .trim_end_matches(')')
                    .rsplit_once('.')
                    .map(|(_, tail)| tail)
                    .unwrap_or(token)
                    .to_string();
                if !node.is_empty() {
                    return Some(node);
                }
            }
        }
    }
    None
}

fn fallback_targets_from_text_blobs(
    texts: &[String],
    error_code: &str,
) -> Vec<ValidateFailingTarget> {
    let mut out: Vec<ValidateFailingTarget> = Vec::new();
    for txt in texts {
        for raw_line in txt.lines() {
            let line = raw_line.trim();
            if line.is_empty() {
                continue;
            }
            let Some(canonical_path) = extract_models_path_hint(line) else {
                continue;
            };
            let Some(target_path) = ValidateTargetPath::parse(canonical_path.clone()).ok() else {
                continue;
            };
            let node_id = node_hint_from_line(line).unwrap_or_else(|| {
                std::path::Path::new(&canonical_path)
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| canonical_path.clone())
            });
            out.push(ValidateFailingTarget {
                node_id,
                target_path,
                error_code: error_code.to_string(),
            });
        }
    }
    out
}

fn build_failing_targets_from_logs(
    logs: &Value,
    errors: &[String],
    failure_class: ValidateFailureClass,
) -> Vec<ValidateFailingTarget> {
    let error_code = failure_class_key(failure_class).to_string();
    let mut out: Vec<ValidateFailingTarget> = crate::data_engineer::dbt_error::extract_failed_models_from_logs(logs)
        .into_iter()
        .filter_map(|fm| {
            let node_id = fm
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())?;
            let canonical_path = fm
                .get("file")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())?;
            let target_path = ValidateTargetPath::parse(canonical_path).ok()?;
            Some(ValidateFailingTarget {
                node_id,
                target_path,
                error_code: error_code.clone(),
            })
        })
        .collect();
    if out.is_empty() {
        // dbt test failures can report only FAIL lines; derive deterministic targets from those lines.
        let runtime_failures = crate::data_engineer::dbt_error::extract_runtime_failures_from_logs(logs);
        for rf in runtime_failures {
            let node_id = rf
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    rf.get("model_hint")
                        .and_then(|v| v.as_str())
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                });
            let canonical_path = rf
                .get("line")
                .and_then(|v| v.as_str())
                .and_then(extract_models_path_hint);
            if let (Some(node_id), Some(canonical_path)) = (node_id, canonical_path) {
                let Some(target_path) = ValidateTargetPath::parse(canonical_path).ok() else {
                    continue;
                };
                out.push(ValidateFailingTarget {
                    node_id,
                    target_path,
                    error_code: error_code.clone(),
                });
            }
        }
    }
    if out.is_empty() {
        // Compile/dbt parser errors often carry model paths in `errors` or compile stderr only.
        let mut blobs: Vec<String> = errors.to_vec();
        for phase in ["compile", "run_or_build"] {
            for stream in ["stdout", "stderr"] {
                if let Some(s) = logs
                    .get(phase)
                    .and_then(|v| v.get(stream))
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    blobs.push(s.to_string());
                }
            }
        }
        out.extend(fallback_targets_from_text_blobs(&blobs, &error_code));
    }
    out.sort_by(|a, b| a.target_path.as_str().cmp(b.target_path.as_str()));
    out.dedup_by(|a, b| a.target_path == b.target_path);
    out
}

pub fn attach_validate_outcome_v2(obs: &mut Value) -> Result<ValidateOutcomeV2, String> {
    let obj = obs
        .as_object_mut()
        .ok_or_else(|| "validate_outcome_v2_contract_error: observation_not_object".to_string())?;
    let ok = obj.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    let compile_ok = obj
        .get("compile_ok")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let run_ok = obj.get("run_ok").and_then(|v| v.as_bool()).unwrap_or(false);
    let errors: Vec<String> = obj
        .get("errors")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let logs = obj.get("logs").cloned().unwrap_or(Value::Null);
    let class = failure_class_from_validate_result(obj);
    let failing_targets = if ok {
        Vec::new()
    } else {
        build_failing_targets_from_logs(&logs, &errors, class)
    };
    if !ok && failing_targets.is_empty() {
        return Err("validate_outcome_v2_contract_error: missing failing_targets for failed validate".to_string());
    }
    let failure_signature = failing_targets.first().map(|t| FailureSignature {
        class,
        node_id: t.node_id.clone(),
        target_path: t.target_path.clone(),
        error_code: t.error_code.clone(),
    });
    let outcome = ValidateOutcomeV2 {
        ok,
        compile_ok,
        run_ok,
        failing_targets,
        failure_signature,
    };
    obj.insert(
        "validate_outcome_v2".to_string(),
        serde_json::to_value(&outcome).map_err(|e| e.to_string())?,
    );
    Ok(outcome)
}

pub fn validate_contract_from_observation(mut obs: Value) -> Result<ValidateObservationContract, String> {
    let outcome_v2 = attach_validate_outcome_v2(&mut obs)?;
    Ok(ValidateObservationContract {
        observation: obs,
        outcome_v2,
    })
}

pub fn validate_event_from_contract(contract: &ValidateObservationContract) -> ControllerEvent {
    let v2 = &contract.outcome_v2;
    if v2.ok && v2.compile_ok && v2.run_ok {
        return ControllerEvent::ValidatePassed;
    }
    let errs: Vec<String> = contract
        .observation
        .get("errors")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let brief = crate::data_engineer::dbt_error::compact_brief(&errs, 6, 1200);
    if v2.failing_targets.is_empty() {
        return ControllerEvent::ValidateContractError {
            reason: "validate_outcome_v2_missing_failing_targets".to_string(),
            brief,
        };
    }
    let Some(signature) = v2.failure_signature.clone() else {
        return ControllerEvent::ValidateContractError {
            reason: "validate_outcome_v2_missing_failure_signature".to_string(),
            brief,
        };
    };
    ControllerEvent::ValidateFailed {
        class: signature.class,
        signature,
        brief,
        failing_targets: v2.failing_targets.clone(),
        compile_ok: v2.compile_ok,
        run_ok: v2.run_ok,
    }
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
        let Ok(target_path) = ValidateTargetPath::parse(canonical_path) else {
            continue;
        };
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
            target_path,
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
    let Ok(target_path) = ValidateTargetPath::parse(canonical_path) else {
        return ControllerEvent::ValidateContractError {
            reason: "validate_outcome_v2_failure_signature_incomplete".to_string(),
            brief,
        };
    };
    if node_id.is_empty() || error_code.is_empty() {
        return ControllerEvent::ValidateContractError {
            reason: "validate_outcome_v2_failure_signature_incomplete".to_string(),
            brief,
        };
    }
    let signature = FailureSignature {
        class,
        node_id,
        target_path,
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

    #[test]
    fn validate_contract_attaches_outcome_for_success() {
        let obs = serde_json::json!({
            "ok": true,
            "compile_ok": true,
            "run_ok": true,
            "errors": []
        });
        let contract = validate_contract_from_observation(obs).expect("contract");
        assert!(contract.outcome_v2.ok);
        assert!(contract.outcome_v2.compile_ok);
        assert!(contract.outcome_v2.run_ok);
        assert!(
            contract
                .observation
                .get("validate_outcome_v2")
                .is_some(),
            "expected validate_outcome_v2 to be attached"
        );
        assert_eq!(
            validate_event_from_contract(&contract),
            ControllerEvent::ValidatePassed
        );
    }

    #[test]
    fn validate_contract_fails_closed_for_failed_validate_without_targets() {
        let obs = serde_json::json!({
            "ok": false,
            "compile_ok": false,
            "run_ok": false,
            "errors": ["Compilation Error"],
            "logs": {}
        });
        let err = validate_contract_from_observation(obs).unwrap_err();
        assert!(
            err.contains("missing failing_targets"),
            "unexpected contract error: {err}"
        );
    }

    #[test]
    fn validate_contract_extracts_targets_from_runtime_fail_lines() {
        let obs = serde_json::json!({
            "ok": false,
            "compile_ok": true,
            "run_ok": false,
            "errors": ["Data test failure"],
            "logs": {
                "run_or_build": {
                    "stdout": "11:19:11  7 of 18 FAIL 2 not_null_stg_orders_order_id (models/staging/stg_orders.yml) [FAIL 2 in 5.89s]\n"
                }
            }
        });
        let contract = validate_contract_from_observation(obs).expect("contract");
        assert_eq!(contract.outcome_v2.failing_targets.len(), 1);
        assert_eq!(
            contract.outcome_v2.failing_targets[0].target_path.as_str(),
            "models/staging/stg_orders.yml"
        );
    }

    #[test]
    fn validate_contract_extracts_targets_from_compile_errors() {
        let obs = serde_json::json!({
            "ok": false,
            "compile_ok": false,
            "run_ok": false,
            "failure_class": "warehouse_config",
            "errors": [
                "Compilation Error in model stg_orders (models/staging/stg_orders.sql)"
            ],
            "logs": {}
        });
        let contract = validate_contract_from_observation(obs).expect("contract");
        assert_eq!(contract.outcome_v2.failing_targets.len(), 1);
        assert_eq!(
            contract.outcome_v2.failing_targets[0].target_path.as_str(),
            "models/staging/stg_orders.sql"
        );
    }

    #[test]
    fn validate_contract_uses_typed_failure_class_field() {
        let obs = serde_json::json!({
            "ok": false,
            "compile_ok": false,
            "run_ok": false,
            "failure_class": "warehouse_config",
            "errors": [
                "Compilation Error in model stg_orders (models/staging/stg_orders.sql)"
            ],
            "logs": {}
        });
        let contract = validate_contract_from_observation(obs).expect("contract");
        let sig = contract
            .outcome_v2
            .failure_signature
            .expect("expected failure signature");
        assert_eq!(sig.class, ValidateFailureClass::WarehouseConfig);
    }
}
