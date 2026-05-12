use std::collections::BTreeSet;
use std::fmt;

use serde::Serialize;

use crate::plan_types::{CleanseTask, ModelTask, OutputFieldSpec};

pub(crate) const SQL_SPEC_DIGEST_PREFIX: &str = "-- skippr-plan-spec-digest:";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ContractVerification {
    pub digest: Option<String>,
    pub drifts: Vec<ContractDrift>,
    pub drift_reasons: Vec<String>,
}

impl ContractVerification {
    pub(crate) fn is_ok(&self) -> bool {
        self.drifts.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ContractDrift {
    SpecDigestMismatch {
        expected: String,
        found: String,
    },
    MissingCurrentSpecDigest,
    MissingImplementationSpec,
    GoldSqlContainsSourceCall,
    RefInputsMismatch {
        expected: BTreeSet<String>,
        found: BTreeSet<String>,
    },
    OutputColumnsMismatch {
        expected: BTreeSet<String>,
        found: BTreeSet<String>,
    },
    FinalSelectColumnsUnavailable(String),
    SchemaParseError(String),
    SchemaModelEntryCount {
        model_name: String,
        found: usize,
    },
    SchemaColumnsMismatch {
        model_name: String,
        expected: BTreeSet<String>,
        found: BTreeSet<String>,
    },
    SchemaTestContradictsSpec {
        model_name: String,
        column_name: String,
        test_name: String,
        reason: String,
    },
    SchemaRelationshipContradictsSpec {
        model_name: String,
        column_name: String,
        reason: String,
    },
}

impl fmt::Display for ContractDrift {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SpecDigestMismatch { expected, found } => {
                write!(f, "spec digest mismatch: expected {expected}, found {found}")
            }
            Self::MissingCurrentSpecDigest => write!(f, "missing current spec digest"),
            Self::MissingImplementationSpec => write!(f, "missing model implementation_spec"),
            Self::GoldSqlContainsSourceCall => {
                write!(f, "gold SQL contains source(); expected ref() inputs only")
            }
            Self::RefInputsMismatch { expected, found } => {
                write!(f, "ref inputs mismatch: expected {:?}, found {:?}", expected, found)
            }
            Self::OutputColumnsMismatch { expected, found } => {
                write!(
                    f,
                    "output columns mismatch: expected {:?}, found {:?}",
                    expected, found
                )
            }
            Self::FinalSelectColumnsUnavailable(e) => {
                write!(f, "could not extract final SELECT columns: {e}")
            }
            Self::SchemaParseError(e) => write!(f, "schema.yml parse error: {e}"),
            Self::SchemaModelEntryCount { model_name, found } => write!(
                f,
                "expected exactly one models/schema.yml entry for {model_name}, found {found}"
            ),
            Self::SchemaColumnsMismatch {
                model_name,
                expected,
                found,
            } => write!(
                f,
                "schema columns mismatch for {model_name}: expected {:?}, found {:?}",
                expected, found
            ),
            Self::SchemaTestContradictsSpec {
                model_name,
                column_name,
                test_name,
                reason,
            } => write!(
                f,
                "schema test contradicts spec for {model_name}.{column_name}: {test_name} ({reason})"
            ),
            Self::SchemaRelationshipContradictsSpec {
                model_name,
                column_name,
                reason,
            } => write!(
                f,
                "schema relationship contradicts spec for {model_name}.{column_name}: {reason}"
            ),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ArtifactContractStatus {
    Current,
    Missing,
    OffContract(Vec<ContractDrift>),
    External,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RepairRoute {
    ReconcileToPlan,
    RepairImplementation,
    CleanStaleArtifact,
    RequestPlanRevision,
    FatalInfraOrConfig,
}

impl ArtifactContractStatus {
    pub(crate) fn repair_route(&self) -> RepairRoute {
        match self {
            Self::Current => RepairRoute::RepairImplementation,
            Self::Missing | Self::OffContract(_) => RepairRoute::ReconcileToPlan,
            Self::External => RepairRoute::CleanStaleArtifact,
        }
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
    let mut drifts = Vec::new();

    if require_current_digest {
        match (
            expected_digest.as_deref(),
            extract_sql_spec_digest(sql).as_deref(),
        ) {
            (Some(expected), Some(actual)) if expected == actual => {}
            (Some(expected), Some(actual)) => drifts.push(ContractDrift::SpecDigestMismatch {
                expected: expected.to_string(),
                found: actual.to_string(),
            }),
            (Some(_), None) => drifts.push(ContractDrift::MissingCurrentSpecDigest),
            (None, _) => drifts.push(ContractDrift::MissingImplementationSpec),
        }
    }

    if crate::naming::contains_source_call(sql) {
        drifts.push(ContractDrift::GoldSqlContainsSourceCall);
    }

    let expected_refs = normalize_set(task.inputs.iter().map(|s| s.as_str()));
    let actual_refs = normalize_set(
        crate::naming::extract_ref_calls(sql)
            .iter()
            .map(|s| s.as_str()),
    );
    if !expected_refs.is_empty() && expected_refs != actual_refs {
        drifts.push(ContractDrift::RefInputsMismatch {
            expected: expected_refs,
            found: actual_refs,
        });
    }

    if let Some(spec) = task.implementation_spec.as_ref() {
        let expected_cols = output_field_names(&spec.output_fields);
        if expected_cols.is_empty() {
            drifts.push(ContractDrift::MissingImplementationSpec);
        } else {
            match crate::tools::files_tool::extract_final_select_output_columns(sql) {
                Ok(actual_cols) => {
                    let actual_cols = normalize_set(actual_cols.iter().map(|s| s.as_str()));
                    if expected_cols != actual_cols {
                        drifts.push(ContractDrift::OutputColumnsMismatch {
                            expected: expected_cols,
                            found: actual_cols,
                        });
                    }
                }
                Err(e) => drifts.push(ContractDrift::FinalSelectColumnsUnavailable(e)),
            }
        }
    } else {
        drifts.push(ContractDrift::MissingImplementationSpec);
    }

    ContractVerification {
        digest: expected_digest,
        drift_reasons: drift_reason_strings(&drifts),
        drifts,
    }
}

pub(crate) fn verify_model_schema_yml_contract(
    model_name: &str,
    expected_fields: &[OutputFieldSpec],
    yml_text: &str,
) -> ContractVerification {
    let mut drifts = Vec::new();
    let expected_cols = output_field_names(expected_fields);
    if expected_cols.is_empty() {
        drifts.push(ContractDrift::MissingImplementationSpec);
    }

    let root: serde_yaml::Value = match serde_yaml::from_str(yml_text) {
        Ok(v) => v,
        Err(e) => {
            let drifts = vec![ContractDrift::SchemaParseError(e.to_string())];
            return ContractVerification {
                digest: None,
                drift_reasons: drift_reason_strings(&drifts),
                drifts,
            };
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
        drifts.push(ContractDrift::SchemaModelEntryCount {
            model_name: model_name.to_string(),
            found: matching.len(),
        });
        return ContractVerification {
            digest: None,
            drift_reasons: drift_reason_strings(&drifts),
            drifts,
        };
    }

    let actual_cols = schema_model_column_names(&matching[0]);
    if !expected_cols.is_empty() && expected_cols != actual_cols {
        drifts.push(ContractDrift::SchemaColumnsMismatch {
            model_name: model_name.to_string(),
            expected: expected_cols,
            found: actual_cols,
        });
    }
    drifts.extend(schema_model_test_drifts(
        model_name,
        expected_fields,
        &matching[0],
    ));

    ContractVerification {
        digest: None,
        drift_reasons: drift_reason_strings(&drifts),
        drifts,
    }
}

fn drift_reason_strings(drifts: &[ContractDrift]) -> Vec<String> {
    drifts.iter().map(ToString::to_string).collect()
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

fn schema_model_test_drifts(
    model_name: &str,
    expected_fields: &[OutputFieldSpec],
    model: &serde_yaml::Mapping,
) -> Vec<ContractDrift> {
    let mut drifts = Vec::new();
    let expected_by_name = expected_fields
        .iter()
        .map(|f| (normalize_name(&f.name), f))
        .collect::<std::collections::BTreeMap<_, _>>();
    let cols = model
        .get(serde_yaml::Value::String("columns".to_string()))
        .and_then(|v| v.as_sequence())
        .cloned()
        .unwrap_or_default();
    for col in cols {
        let Some(map) = col.as_mapping() else {
            continue;
        };
        let Some(column_name) = map
            .get(serde_yaml::Value::String("name".to_string()))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let Some(field) = expected_by_name.get(&normalize_name(column_name)) else {
            continue;
        };
        let tests = map
            .get(serde_yaml::Value::String("tests".to_string()))
            .and_then(|v| v.as_sequence())
            .cloned()
            .unwrap_or_default();
        for test in tests {
            let Some(test_name) = schema_test_name(&test) else {
                continue;
            };
            if field.nullable && test_name == "not_null" {
                drifts.push(ContractDrift::SchemaTestContradictsSpec {
                    model_name: model_name.to_string(),
                    column_name: column_name.to_string(),
                    test_name,
                    reason: "field is nullable in the active plan".to_string(),
                });
            } else if test_name == "relationships" && field.nullable {
                drifts.push(ContractDrift::SchemaRelationshipContradictsSpec {
                    model_name: model_name.to_string(),
                    column_name: column_name.to_string(),
                    reason:
                        "nullable output field cannot require a complete relationship to another model"
                            .to_string(),
                });
            }
        }
    }
    drifts
}

fn schema_test_name(test: &serde_yaml::Value) -> Option<String> {
    match test {
        serde_yaml::Value::String(s) => Some(normalize_name(s)),
        serde_yaml::Value::Mapping(map) => {
            if map.len() != 1 {
                return None;
            }
            map.keys()
                .next()
                .and_then(|k| k.as_str())
                .map(normalize_name)
        }
        _ => None,
    }
}

fn normalize_name(s: &str) -> String {
    s.trim()
        .trim_matches('"')
        .trim_matches('`')
        .to_ascii_lowercase()
}

fn normalize_set<'a>(values: impl IntoIterator<Item = &'a str>) -> BTreeSet<String> {
    values
        .into_iter()
        .map(normalize_name)
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
            lineage: vec![],
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

    #[test]
    fn schema_contract_rejects_not_null_on_nullable_field() {
        let yml = r#"
version: 2
models:
  - name: agg_daily_product_sales
    columns:
      - name: product_id
        tests:
          - not_null
"#;

        let check = verify_model_schema_yml_contract(
            "agg_daily_product_sales",
            &[field("product_id")],
            yml,
        );

        assert!(!check.is_ok());
        assert!(check
            .drifts
            .iter()
            .any(|d| matches!(d, ContractDrift::SchemaTestContradictsSpec { .. })));
    }

    #[test]
    fn schema_contract_rejects_relationship_on_nullable_field() {
        let yml = r#"
version: 2
models:
  - name: agg_daily_product_sales
    columns:
      - name: product_id
        tests:
          - relationships:
              to: ref('dim_products')
              field: PRODUCT_ID
"#;

        let check = verify_model_schema_yml_contract(
            "agg_daily_product_sales",
            &[field("product_id")],
            yml,
        );

        assert!(!check.is_ok());
        assert!(check
            .drifts
            .iter()
            .any(|d| matches!(d, ContractDrift::SchemaRelationshipContradictsSpec { .. })));
    }
}
