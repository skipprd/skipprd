use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct TruthSnapshot {
    #[serde(default)]
    pub raw_sources: BTreeMap<String, RelationTruth>,
    #[serde(default)]
    pub staging_models: BTreeMap<String, RelationTruth>,
    #[serde(default)]
    pub gold_models: BTreeMap<String, RelationTruth>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RelationTruth {
    pub name: String,
    pub path: Option<String>,
    #[serde(default)]
    pub relation_fqn: Option<String>,
    #[serde(default)]
    pub columns: Vec<ColumnTruth>,
    pub provenance: TruthProvenance,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ColumnTruth {
    pub name: String,
    pub data_type: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TruthProvenance {
    Warehouse,
    Catalog,
    DbtManifest,
    SchemaYaml,
    DbtSql,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct CandidateRelationTruth {
    path: Option<String>,
    relation_fqn: Option<String>,
    columns: Vec<ColumnTruth>,
}

impl RelationTruth {
    pub(crate) fn to_source_columns(&self) -> Vec<crate::plan_types::SourceColumnDef> {
        self.columns
            .iter()
            .map(|column| crate::plan_types::SourceColumnDef {
                name: column.name.clone(),
                data_type: column
                    .data_type
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
            })
            .collect()
    }
}

impl TruthSnapshot {
    pub(crate) async fn build_for_phase(
        ctx: &react_core::agent::AgentCtx,
        phase: crate::control_flow::Phase,
    ) -> Result<Self, String> {
        let mut snapshot = Self::default();
        if matches!(
            phase,
            crate::control_flow::Phase::ModelPlan
                | crate::control_flow::Phase::ModelAuthor
                | crate::control_flow::Phase::ModelValidate
                | crate::control_flow::Phase::ModelReview
        ) {
            let (staging_models, warnings) = build_staging_truth(ctx).await?;
            snapshot.staging_models = staging_models;
            snapshot.warnings = warnings;
        }
        Ok(snapshot)
    }

    pub(crate) fn relation(&self, name: &str) -> Option<&RelationTruth> {
        let name = name.trim();
        self.staging_models
            .get(name)
            .or_else(|| self.gold_models.get(name))
            .or_else(|| self.raw_sources.get(name))
    }

    pub(crate) fn source_schema_for_model_inputs(
        &self,
        input_names: &BTreeSet<String>,
    ) -> crate::plan_types::SourceSchema {
        let mut out = crate::plan_types::SourceSchema::new();
        for input_name in input_names {
            let key = input_name.trim();
            if key.is_empty() {
                continue;
            }
            let Some(relation) = self.relation(key) else {
                continue;
            };
            let cols = relation.to_source_columns();
            if !cols.is_empty() {
                out.insert(key.to_string(), cols);
            }
        }
        out
    }
}

async fn build_staging_truth(
    ctx: &react_core::agent::AgentCtx,
) -> Result<(BTreeMap<String, RelationTruth>, Vec<String>), String> {
    let discovered = crate::dataset_truth::discover_staging_models_from_storage(ctx).await;
    let mut model_names = discovered.allowed_models;
    let mut warnings = discovered.warnings;

    let project_facts = crate::dbt_project_snapshot::scan_project_files(ctx).await?;
    let manifest_models = manifest_models_from_project(&project_facts);
    model_names.extend(manifest_models.keys().cloned());

    let yaml_models = yaml_models_from_project(&project_facts);
    model_names.extend(yaml_models.keys().cloned());

    let sql_models = sql_models_from_project(&project_facts);
    model_names.extend(sql_models.keys().cloned());

    let mut warehouse_schemas = crate::plan_types::SourceSchema::new();
    crate::dataset_truth::record_staging_output_schemas(ctx, &model_names, &mut warehouse_schemas)
        .await;
    let default_prefix = crate::dataset_truth::staging_relation_prefix(ctx);

    let staging_models = assemble_relation_truths(
        &model_names,
        &warehouse_schemas,
        &manifest_models,
        &yaml_models,
        &sql_models,
        default_prefix.as_deref(),
        &mut warnings,
    );
    Ok((staging_models, warnings))
}

fn manifest_models_from_project(
    facts: &crate::dbt_project_snapshot::ProjectFileFacts,
) -> BTreeMap<String, CandidateRelationTruth> {
    facts
        .manifest_models
        .iter()
        .filter(|(name, model)| {
            name.starts_with("stg_")
                && model
                    .path
                    .as_deref()
                    .map(|path| path.starts_with("models/staging/"))
                    .unwrap_or(false)
        })
        .map(|(name, model)| {
            (
                name.clone(),
                CandidateRelationTruth {
                    path: model.path.clone(),
                    relation_fqn: model.relation_fqn.clone(),
                    columns: project_columns_to_truth(&model.columns),
                },
            )
        })
        .collect()
}

fn yaml_models_from_project(
    facts: &crate::dbt_project_snapshot::ProjectFileFacts,
) -> BTreeMap<String, CandidateRelationTruth> {
    let mut out = BTreeMap::new();
    for model in facts.schema_models.values() {
        if !model.model_name.starts_with("stg_") {
            continue;
        }
        upsert_candidate_truth(
            &mut out,
            &model.model_name,
            CandidateRelationTruth {
                path: Some(model.path.clone()),
                relation_fqn: None,
                columns: project_columns_to_truth(&model.columns),
            },
        );
    }
    out
}

fn sql_models_from_project(
    facts: &crate::dbt_project_snapshot::ProjectFileFacts,
) -> BTreeMap<String, CandidateRelationTruth> {
    let mut out = BTreeMap::new();
    for model in facts.sql_models.values() {
        if !model.model_name.starts_with("stg_") || !model.path.starts_with("models/staging/") {
            continue;
        }
        upsert_candidate_truth(
            &mut out,
            &model.model_name,
            CandidateRelationTruth {
                path: Some(model.path.clone()),
                relation_fqn: None,
                columns: model
                    .columns
                    .iter()
                    .map(|name| ColumnTruth {
                        name: name.clone(),
                        data_type: None,
                    })
                    .collect(),
            },
        );
    }
    out
}

fn project_columns_to_truth(
    columns: &[crate::dbt_project_snapshot::ProjectColumn],
) -> Vec<ColumnTruth> {
    columns
        .iter()
        .map(|column| ColumnTruth {
            name: column.name.clone(),
            data_type: column.data_type.clone(),
        })
        .collect()
}

fn assemble_relation_truths(
    model_names: &BTreeSet<String>,
    warehouse_schemas: &crate::plan_types::SourceSchema,
    manifest_models: &BTreeMap<String, CandidateRelationTruth>,
    yaml_models: &BTreeMap<String, CandidateRelationTruth>,
    sql_models: &BTreeMap<String, CandidateRelationTruth>,
    default_prefix: Option<&str>,
    warnings: &mut Vec<String>,
) -> BTreeMap<String, RelationTruth> {
    let mut out = BTreeMap::new();
    for name in model_names {
        let manifest = manifest_models.get(name);
        let yaml = yaml_models.get(name);
        let sql = sql_models.get(name);
        let warehouse_cols = warehouse_schemas
            .get(name.as_str())
            .cloned()
            .unwrap_or_default();

        let default_path = Some(format!("models/staging/{name}.sql"));
        let default_relation_fqn = default_prefix
            .map(str::trim)
            .filter(|prefix| !prefix.is_empty())
            .map(|prefix| format!("{}.{}", prefix, name));

        let path = manifest
            .and_then(|m| m.path.clone())
            .or_else(|| sql.as_ref().and_then(|s| s.path.clone()))
            .or_else(|| yaml.as_ref().and_then(|y| y.path.clone()))
            .or(default_path);
        let relation_fqn = manifest
            .and_then(|m| m.relation_fqn.clone())
            .or(default_relation_fqn);

        let (columns, provenance) = if !warehouse_cols.is_empty() {
            (
                warehouse_cols
                    .iter()
                    .map(source_column_def_to_truth)
                    .collect::<Vec<_>>(),
                TruthProvenance::Warehouse,
            )
        } else if let Some(manifest) = manifest.filter(|m| !m.columns.is_empty()) {
            (manifest.columns.clone(), TruthProvenance::DbtManifest)
        } else if let Some(yaml) = yaml.filter(|y| !y.columns.is_empty()) {
            (yaml.columns.clone(), TruthProvenance::SchemaYaml)
        } else if let Some(sql) = sql.filter(|s| !s.columns.is_empty()) {
            (sql.columns.clone(), TruthProvenance::DbtSql)
        } else if manifest.is_some() {
            warnings.push(format!(
                "truth_snapshot: manifest discovered staging model '{}' but no columns were available from warehouse, manifest, YAML, or SQL",
                name
            ));
            (Vec::new(), TruthProvenance::DbtManifest)
        } else if yaml.is_some() {
            warnings.push(format!(
                "truth_snapshot: schema YAML discovered staging model '{}' but no columns were declared",
                name
            ));
            (Vec::new(), TruthProvenance::SchemaYaml)
        } else {
            (Vec::new(), TruthProvenance::DbtSql)
        };

        out.insert(
            name.clone(),
            RelationTruth {
                name: name.clone(),
                path,
                relation_fqn,
                columns,
                provenance,
            },
        );
    }
    out
}

fn upsert_candidate_truth(
    out: &mut BTreeMap<String, CandidateRelationTruth>,
    name: &str,
    next: CandidateRelationTruth,
) {
    let entry = out.entry(name.to_string()).or_default();
    if entry.path.is_none() {
        entry.path = next.path;
    }
    if entry.relation_fqn.is_none() {
        entry.relation_fqn = next.relation_fqn;
    }
    if entry.columns.is_empty() && !next.columns.is_empty() {
        entry.columns = next.columns;
    }
}

fn source_column_def_to_truth(column: &crate::plan_types::SourceColumnDef) -> ColumnTruth {
    ColumnTruth {
        name: column.name.clone(),
        data_type: Some(column.data_type.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, data_type: Option<&str>) -> ColumnTruth {
        ColumnTruth {
            name: name.to_string(),
            data_type: data_type.map(|value| value.to_string()),
        }
    }

    #[test]
    fn source_schema_for_model_inputs_uses_non_empty_truth_columns() {
        let snapshot = TruthSnapshot {
            staging_models: BTreeMap::from([(
                "stg_raw_bike_hire".to_string(),
                RelationTruth {
                    name: "stg_raw_bike_hire".to_string(),
                    path: Some("models/staging/stg_raw_bike_hire.sql".to_string()),
                    relation_fqn: Some("db.schema.stg_raw_bike_hire".to_string()),
                    columns: vec![
                        col("RIDER_ID", Some("string")),
                        col("BIKE_ID", Some("string")),
                    ],
                    provenance: TruthProvenance::SchemaYaml,
                },
            )]),
            ..Default::default()
        };
        let inputs = BTreeSet::from(["stg_raw_bike_hire".to_string()]);
        let got = snapshot.source_schema_for_model_inputs(&inputs);
        assert_eq!(got["stg_raw_bike_hire"].len(), 2);
        assert_eq!(got["stg_raw_bike_hire"][0].name, "RIDER_ID");
    }

    #[test]
    fn assemble_relation_truths_prefers_warehouse_then_manifest_then_yaml_then_sql() {
        let names = BTreeSet::from(["stg_orders".to_string(), "stg_payments".to_string()]);
        let warehouse = crate::plan_types::SourceSchema::from([(
            "stg_orders".to_string(),
            vec![crate::plan_types::SourceColumnDef {
                name: "ORDER_ID".to_string(),
                data_type: "bigint".to_string(),
            }],
        )]);
        let manifest = BTreeMap::from([(
            "stg_payments".to_string(),
            CandidateRelationTruth {
                path: Some("models/staging/stg_payments.sql".to_string()),
                relation_fqn: Some("db.schema.stg_payments".to_string()),
                columns: vec![col("PAYMENT_ID", Some("bigint"))],
            },
        )]);
        let yaml = BTreeMap::from([(
            "stg_orders".to_string(),
            CandidateRelationTruth {
                path: Some("models/staging/stg_orders.yml".to_string()),
                relation_fqn: None,
                columns: vec![col("order_id", Some("string"))],
            },
        )]);
        let sql = BTreeMap::from([(
            "stg_orders".to_string(),
            CandidateRelationTruth {
                path: Some("models/staging/stg_orders.sql".to_string()),
                relation_fqn: None,
                columns: vec![col("order_id", None)],
            },
        )]);

        let mut warnings = Vec::new();
        let got = assemble_relation_truths(
            &names,
            &warehouse,
            &manifest,
            &yaml,
            &sql,
            Some("db.schema"),
            &mut warnings,
        );
        assert_eq!(got["stg_orders"].provenance, TruthProvenance::Warehouse);
        assert_eq!(got["stg_orders"].columns[0].name, "ORDER_ID");
        assert_eq!(got["stg_payments"].provenance, TruthProvenance::DbtManifest);
        assert!(warnings.is_empty());
    }

    #[test]
    fn assemble_relation_truths_uses_yaml_when_warehouse_is_empty() {
        let names = BTreeSet::from(["stg_raw_bike_hire".to_string()]);
        let mut warnings = Vec::new();
        let got = assemble_relation_truths(
            &names,
            &crate::plan_types::SourceSchema::new(),
            &BTreeMap::new(),
            &BTreeMap::from([(
                "stg_raw_bike_hire".to_string(),
                CandidateRelationTruth {
                    path: Some("models/staging/stg_raw_bike_hire.yml".to_string()),
                    relation_fqn: None,
                    columns: vec![
                        col("RIDER_ID", Some("string")),
                        col("EVENT_DATE", Some("date")),
                    ],
                },
            )]),
            &BTreeMap::from([(
                "stg_raw_bike_hire".to_string(),
                CandidateRelationTruth {
                    path: Some("models/staging/stg_raw_bike_hire.sql".to_string()),
                    relation_fqn: None,
                    columns: vec![col("RIDER_ID", None)],
                },
            )]),
            Some("db.schema"),
            &mut warnings,
        );
        assert_eq!(
            got["stg_raw_bike_hire"].provenance,
            TruthProvenance::SchemaYaml
        );
        assert_eq!(got["stg_raw_bike_hire"].columns.len(), 2);
        assert!(warnings.is_empty());
    }
}
