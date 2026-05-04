use std::collections::BTreeSet;

use serde::Serialize;

use crate::plan_types::{CleanseTask, ModelTask, OutputFieldSpec};

pub(crate) const SQL_SPEC_DIGEST_PREFIX: &str = "-- skippr-plan-spec-digest:";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ContractVerification {
    pub digest: Option<String>,
    pub drift_reasons: Vec<String>,
}

impl ContractVerification {
    pub(crate) fn is_ok(&self) -> bool {
        self.drift_reasons.is_empty()
    }
}

pub(crate) fn add_sql_spec_digest(sql: &str, digest: Option<&str>) -> String {
    let Some(digest) = digest.map(str::trim).filter(|d| !d.is_empty()) else {
        return sql.to_string();
    };
    let body = strip_sql_spec_digest(sql).trim_start().to_string();
    format!("{SQL_SPEC_DIGEST_PREFIX} {digest}\n{body}")
}

pub(crate) fn strip_sql_spec_digest(sql: &str) -> String {
    sql.lines()
        .filter(|line| !line.trim_start().starts_with(SQL_SPEC_DIGEST_PREFIX))
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn extract_sql_spec_digest(sql: &str) -> Option<String> {
    sql.lines().find_map(|line| {
        let t = line.trim_start();
        t.strip_prefix(SQL_SPEC_DIGEST_PREFIX)
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(str::to_string)
    })
}

pub(crate) fn cleanse_task_spec_digest(_plan_key: &str, task: &CleanseTask) -> Option<String> {
    let spec = task.implementation_spec.as_ref()?;
    Some(spec_digest(&serde_json::json!({
        "kind": "cleanse",
        "dataset_id": task.dataset_id,
        "expected_model_path": task.expected_model_path,
        "implementation_spec": spec,
    })))
}

pub(crate) fn model_task_spec_digest(_plan_key: &str, task: &ModelTask) -> Option<String> {
    let spec = task.implementation_spec.as_ref()?;
    Some(spec_digest(&serde_json::json!({
        "kind": "model",
        "name": task.name,
        "folder": task.folder,
        "inputs": task.inputs,
        "expected_model_path": task.expected_model_path,
        "implementation_spec": spec,
    })))
}

fn spec_digest(value: &impl Serialize) -> String {
    let text = serde_json::to_string(value).unwrap_or_default();
    react_core::llm_observability::sha256_hex_str(&text)
}

pub(crate) fn verify_model_sql_contract(
    plan_key: &str,
    task: &ModelTask,
    sql: &str,
    require_current_digest: bool,
) -> ContractVerification {
    let expected_digest = model_task_spec_digest(plan_key, task);
    let mut reasons = Vec::new();

    if require_current_digest {
        match (
            expected_digest.as_deref(),
            extract_sql_spec_digest(sql).as_deref(),
        ) {
            (Some(expected), Some(actual)) if expected == actual => {}
            (Some(expected), Some(actual)) => reasons.push(format!(
                "spec digest mismatch: expected {expected}, found {actual}"
            )),
            (Some(_), None) => reasons.push("missing current spec digest".to_string()),
            (None, _) => reasons.push("missing model implementation_spec".to_string()),
        }
    }

    if crate::naming::contains_source_call(sql) {
        reasons.push("gold SQL contains source(); expected ref() inputs only".to_string());
    }

    let expected_refs = normalize_set(task.inputs.iter().map(|s| s.as_str()));
    let actual_refs = normalize_set(
        crate::naming::extract_ref_calls(sql)
            .iter()
            .map(|s| s.as_str()),
    );
    if !expected_refs.is_empty() && expected_refs != actual_refs {
        reasons.push(format!(
            "ref inputs mismatch: expected {:?}, found {:?}",
            expected_refs, actual_refs
        ));
    }

    if let Some(spec) = task.implementation_spec.as_ref() {
        let expected_cols = output_field_names(&spec.output_fields);
        if expected_cols.is_empty() {
            reasons.push("implementation_spec.output_fields is empty".to_string());
        } else {
            match crate::tools::files_tool::extract_final_select_output_columns(sql) {
                Ok(actual_cols) => {
                    let actual_cols = normalize_set(actual_cols.iter().map(|s| s.as_str()));
                    if expected_cols != actual_cols {
                        reasons.push(format!(
                            "output columns mismatch: expected {:?}, found {:?}",
                            expected_cols, actual_cols
                        ));
                    }
                }
                Err(e) => reasons.push(format!("could not extract final SELECT columns: {e}")),
            }
        }
    } else {
        reasons.push("missing model implementation_spec".to_string());
    }

    ContractVerification {
        digest: expected_digest,
        drift_reasons: reasons,
    }
}

pub(crate) fn verify_model_schema_yml_contract(
    model_name: &str,
    expected_fields: &[OutputFieldSpec],
    yml_text: &str,
) -> ContractVerification {
    let mut reasons = Vec::new();
    let expected_cols = output_field_names(expected_fields);
    if expected_cols.is_empty() {
        reasons.push("implementation_spec.output_fields is empty".to_string());
    }

    let root: serde_yaml::Value = match serde_yaml::from_str(yml_text) {
        Ok(v) => v,
        Err(e) => {
            return ContractVerification {
                digest: None,
                drift_reasons: vec![format!("schema.yml parse error: {e}")],
            }
        }
    };
    let models = root
        .as_mapping()
        .and_then(|m| m.get(serde_yaml::Value::String("models".to_string())))
        .and_then(|v| v.as_sequence())
        .cloned()
        .unwrap_or_default();

    let mut matching = Vec::new();
    for model in models {
        let Some(map) = model.as_mapping() else {
            continue;
        };
        let name = map
            .get(serde_yaml::Value::String("name".to_string()))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if name == model_name {
            matching.push(map.clone());
        }
    }

    if matching.len() != 1 {
        reasons.push(format!(
            "expected exactly one models/schema.yml entry for {model_name}, found {}",
            matching.len()
        ));
        return ContractVerification {
            digest: None,
            drift_reasons: reasons,
        };
    }

    let actual_cols = schema_model_column_names(&matching[0]);
    if !expected_cols.is_empty() && expected_cols != actual_cols {
        reasons.push(format!(
            "schema columns mismatch for {model_name}: expected {:?}, found {:?}",
            expected_cols, actual_cols
        ));
    }

    ContractVerification {
        digest: None,
        drift_reasons: reasons,
    }
}

pub(crate) fn model_sql_drift_reasons(plan_key: &str, task: &ModelTask, sql: &str) -> Vec<String> {
    verify_model_sql_contract(plan_key, task, sql, true).drift_reasons
}

pub(crate) fn model_sql_is_high_drift(reasons: &[String]) -> bool {
    reasons.iter().any(|r| {
        r.contains("missing current spec digest")
            || r.contains("spec digest mismatch")
            || r.contains("output columns mismatch")
            || r.contains("ref inputs mismatch")
    }) || reasons.len() >= 2
}

fn output_field_names(fields: &[OutputFieldSpec]) -> BTreeSet<String> {
    normalize_set(fields.iter().map(|f| f.name.as_str()))
}

fn schema_model_column_names(model: &serde_yaml::Mapping) -> BTreeSet<String> {
    let cols = model
        .get(serde_yaml::Value::String("columns".to_string()))
        .and_then(|v| v.as_sequence())
        .cloned()
        .unwrap_or_default();
    normalize_set(cols.iter().filter_map(|c| {
        c.as_mapping()
            .and_then(|m| m.get(serde_yaml::Value::String("name".to_string())))
            .and_then(|v| v.as_str())
    }))
}

fn normalize_set<'a>(values: impl IntoIterator<Item = &'a str>) -> BTreeSet<String> {
    values
        .into_iter()
        .map(|s| {
            s.trim()
                .trim_matches('"')
                .trim_matches('`')
                .to_ascii_lowercase()
        })
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan_types::{FieldKind, ModelFolder, ModelImplementationSpec};

    fn field(name: &str) -> OutputFieldSpec {
        OutputFieldSpec {
            name: name.to_string(),
            kind: FieldKind::Clean,
            source_columns: vec![name.to_string()],
            expression: name.to_string(),
            data_type: None,
            nullable: true,
            description: None,
        }
    }

    fn model_task() -> ModelTask {
        ModelTask {
            name: "fct_orders".to_string(),
            folder: ModelFolder::Marts,
            goal: "orders fact".to_string(),
            inputs: vec!["stg_orders".to_string()],
            expected_model_path: Some("models/marts/fct_orders.sql".to_string()),
            invariants: vec![],
            implementation_spec: Some(ModelImplementationSpec {
                spec_version: 1,
                grain: "1 row per order".to_string(),
                inputs: vec!["stg_orders".to_string()],
                joins: vec![],
                metrics: vec![],
                output_fields: vec![field("order_id"), field("total_amount")],
                assumptions: vec![],
                evidence_claim_refs: vec![],
            }),
            source_schema: vec![],
            grounded_inputs: vec![],
            status: Default::default(),
            checklist: vec![],
        }
    }

    #[test]
    fn model_task_spec_digest_is_stable_across_plan_keys() {
        let task = model_task();

        let digest_a = model_task_spec_digest("plan-a", &task).unwrap();
        let digest_b = model_task_spec_digest("plan-b", &task).unwrap();

        assert_eq!(digest_a, digest_b);
    }

    #[test]
    fn model_sql_contract_rejects_wrong_output_alias() {
        let task = model_task();
        let digest = model_task_spec_digest("plan1", &task).unwrap();
        let sql = add_sql_spec_digest(
            "select\n  order_id,\n  total_amount as order_total_amount\nfrom {{ ref('stg_orders') }}",
            Some(&digest),
        );

        let check = verify_model_sql_contract("plan1", &task, &sql, true);

        assert!(!check.is_ok());
        assert!(check
            .drift_reasons
            .iter()
            .any(|r| r.contains("output columns mismatch")));
    }

    #[test]
    fn model_sql_contract_rejects_missing_current_digest() {
        let task = model_task();
        let sql = "select\n  order_id,\n  total_amount\nfrom {{ ref('stg_orders') }}";

        let check = verify_model_sql_contract("plan1", &task, sql, true);

        assert!(!check.is_ok());
        assert!(check
            .drift_reasons
            .iter()
            .any(|r| r.contains("missing current spec digest")));
    }

    #[test]
    fn schema_contract_requires_matching_columns() {
        let yml = r#"
version: 2
models:
  - name: fct_orders
    columns:
      - name: order_id
      - name: order_total_amount
"#;

        let check = verify_model_schema_yml_contract(
            "fct_orders",
            &[field("order_id"), field("total_amount")],
            yml,
        );

        assert!(!check.is_ok());
        assert!(check
            .drift_reasons
            .iter()
            .any(|r| r.contains("schema columns mismatch")));
    }
}
