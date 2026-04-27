use serde_json::Value;

pub use crate::domain_types::{ControllerEvent, ValidateObservationContract, ValidateOutcomeV2};

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
    let outcome = ValidateOutcomeV2 {
        ok,
        compile_ok,
        run_ok,
    };
    obj.insert(
        "validate_outcome_v2".to_string(),
        serde_json::to_value(&outcome).map_err(|e| e.to_string())?,
    );
    Ok(outcome)
}

pub fn validate_contract_from_observation(
    mut obs: Value,
) -> Result<ValidateObservationContract, String> {
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
    let brief = crate::dbt_error::compact_brief(&errs, 6, 1200);
    if brief.trim().is_empty() {
        return ControllerEvent::ValidateContractError {
            reason: "validate_outcome_v2_empty_brief".to_string(),
            brief: "dbt_validate failed with no error text.".to_string(),
        };
    }
    let failure_hash = crate::progress_controller::sha256_hex(brief.trim());
    ControllerEvent::ValidateFailed {
        brief,
        failure_hash,
        compile_ok: v2.compile_ok,
        run_ok: v2.run_ok,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            contract.observation.get("validate_outcome_v2").is_some(),
            "expected validate_outcome_v2 to be attached"
        );
        assert_eq!(
            validate_event_from_contract(&contract),
            ControllerEvent::ValidatePassed
        );
    }

    #[test]
    fn validate_contract_produces_failed_event_for_compile_errors() {
        let obs = serde_json::json!({
            "ok": false,
            "compile_ok": false,
            "run_ok": false,
            "errors": [
                "Compilation Error in model stg_orders (models/staging/stg_orders.sql)"
            ],
            "logs": {}
        });
        let contract = validate_contract_from_observation(obs).expect("contract");
        let event = validate_event_from_contract(&contract);
        match event {
            ControllerEvent::ValidateFailed {
                brief,
                failure_hash,
                compile_ok,
                run_ok,
            } => {
                assert!(brief.contains("Compilation Error"));
                assert!(!failure_hash.is_empty());
                assert!(!compile_ok);
                assert!(!run_ok);
            }
            other => panic!("expected ValidateFailed, got {:?}", other),
        }
    }

    #[test]
    fn validate_contract_produces_failed_event_for_runtime_errors() {
        let obs = serde_json::json!({
            "ok": false,
            "compile_ok": true,
            "run_ok": false,
            "errors": ["Data test failure"],
            "logs": {}
        });
        let contract = validate_contract_from_observation(obs).expect("contract");
        let event = validate_event_from_contract(&contract);
        match event {
            ControllerEvent::ValidateFailed {
                compile_ok, run_ok, ..
            } => {
                assert!(compile_ok);
                assert!(!run_ok);
            }
            other => panic!("expected ValidateFailed, got {:?}", other),
        }
    }
}
