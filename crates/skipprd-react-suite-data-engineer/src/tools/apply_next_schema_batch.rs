use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use crate::providers::DatasetCatalogProvider;
use react_core::agent::AgentCtx;
use react_core::tools::Tool;

use crate::chunk_progress_contract;
use crate::controller_kernel;
use crate::naming;
use crate::plan;
use crate::project_fs;
use crate::references::DatasetRef;
use crate::tools::files_tool;

#[derive(Clone, Debug, Default)]
struct ExistingModelMeta {
    description: Option<String>,
    columns: BTreeMap<String, ExistingColumnMeta>,
}

#[derive(Clone, Debug, Default)]
struct ExistingColumnMeta {
    description: Option<String>,
    data_type: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct SchemaDoc {
    version: i32,
    models: Vec<SchemaModelDoc>,
}

#[derive(Clone, Debug, Serialize)]
struct SchemaModelDoc {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    config: Option<SchemaModelConfig>,
    columns: Vec<SchemaColumnDoc>,
}

#[derive(Clone, Debug, Serialize)]
struct SchemaModelConfig {
    contract: SchemaContractConfig,
}

#[derive(Clone, Debug, Serialize)]
struct SchemaContractConfig {
    enforced: bool,
}

#[derive(Clone, Debug, Serialize)]
struct SchemaColumnDoc {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data_type: Option<String>,
}

fn escape_yaml_doc_preamble(s: String) -> String {
    // serde_yaml may emit a leading `---\n`; keep stored files clean and consistent.
    s.trim_start_matches("---\n").to_string()
}

fn yaml_str<'a>(mapping: &'a serde_yaml::Mapping, key: &str) -> Option<&'a str> {
    mapping
        .get(serde_yaml::Value::String(key.to_string()))
        .and_then(|value| value.as_str())
}

fn existing_model_meta(yml_text: Option<&str>, model_name: &str) -> ExistingModelMeta {
    let Some(yml_text) = yml_text else {
        return ExistingModelMeta::default();
    };
    let Ok(root) = serde_yaml::from_str::<serde_yaml::Value>(yml_text) else {
        return ExistingModelMeta::default();
    };
    let Some(models) = root
        .as_mapping()
        .and_then(|mapping| mapping.get(serde_yaml::Value::String("models".to_string())))
        .and_then(|value| value.as_sequence())
    else {
        return ExistingModelMeta::default();
    };
    for model in models {
        let Some(mapping) = model.as_mapping() else {
            continue;
        };
        if yaml_str(mapping, "name").map(str::trim) != Some(model_name) {
            continue;
        }
        let mut meta = ExistingModelMeta {
            description: yaml_str(mapping, "description")
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| value.to_string()),
            columns: BTreeMap::new(),
        };
        let Some(columns) = mapping
            .get(serde_yaml::Value::String("columns".to_string()))
            .and_then(|value| value.as_sequence())
        else {
            return meta;
        };
        for column in columns {
            let Some(column_map) = column.as_mapping() else {
                continue;
            };
            let Some(name) = yaml_str(column_map, "name")
                .map(str::trim)
                .filter(|value| !value.is_empty())
            else {
                continue;
            };
            meta.columns.insert(
                name.to_string(),
                ExistingColumnMeta {
                    description: yaml_str(column_map, "description")
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .map(|value| value.to_string()),
                    data_type: yaml_str(column_map, "data_type")
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .map(|value| value.to_string()),
                },
            );
        }
        return meta;
    }
    ExistingModelMeta::default()
}

fn output_field_meta(
    fields: &[crate::plan_types::OutputFieldSpec],
) -> BTreeMap<String, ExistingColumnMeta> {
    fields
        .iter()
        .map(|field| {
            (
                field.name.trim().to_string(),
                ExistingColumnMeta {
                    description: field
                        .description
                        .as_ref()
                        .map(|value| value.trim().to_string())
                        .filter(|value| !value.is_empty()),
                    data_type: field
                        .data_type
                        .as_ref()
                        .map(|value| value.trim().to_string())
                        .filter(|value| !value.is_empty()),
                },
            )
        })
        .filter(|(name, _)| !name.is_empty())
        .collect()
}

fn schema_model_doc(
    model_name: &str,
    description: Option<String>,
    columns: &[String],
    existing: &ExistingModelMeta,
    planned: &BTreeMap<String, ExistingColumnMeta>,
    staging_contract_config: bool,
) -> SchemaModelDoc {
    SchemaModelDoc {
        name: model_name.to_string(),
        description: description.or_else(|| existing.description.clone()),
        config: staging_contract_config.then_some(SchemaModelConfig {
            contract: SchemaContractConfig { enforced: false },
        }),
        columns: columns
            .iter()
            .map(|name| {
                let existing_col = existing.columns.get(name);
                let planned_col = planned.get(name);
                SchemaColumnDoc {
                    name: name.clone(),
                    description: existing_col
                        .and_then(|meta| meta.description.clone())
                        .or_else(|| planned_col.and_then(|meta| meta.description.clone())),
                    data_type: existing_col
                        .and_then(|meta| meta.data_type.clone())
                        .or_else(|| planned_col.and_then(|meta| meta.data_type.clone())),
                }
            })
            .collect(),
    }
}

fn render_staging_schema_yml(
    model_name: &str,
    dataset_id: &str,
    columns: &[String],
    existing_text: Option<&str>,
    planned_fields: &[crate::plan_types::OutputFieldSpec],
) -> Result<String, String> {
    let existing = existing_model_meta(existing_text, model_name);
    let planned = output_field_meta(planned_fields);
    let doc = SchemaDoc {
        version: 2,
        models: vec![schema_model_doc(
            model_name,
            Some(format!("Staging model for {dataset_id}.")),
            columns,
            &existing,
            &planned,
            true,
        )],
    };
    serde_yaml::to_string(&doc)
        .map(escape_yaml_doc_preamble)
        .map_err(|e| format!("failed to render staging schema YAML: {e}"))
}

fn schema_model_value(model: SchemaModelDoc) -> Result<serde_yaml::Value, String> {
    serde_yaml::to_value(model).map_err(|e| format!("failed to encode model schema entry: {e}"))
}

fn merge_models_schema_yml(
    existing_text: Option<&str>,
    models: Vec<SchemaModelDoc>,
) -> Result<String, String> {
    let touched: HashSet<String> = models.iter().map(|model| model.name.clone()).collect();
    let mut root = existing_text
        .and_then(|text| serde_yaml::from_str::<serde_yaml::Value>(text).ok())
        .unwrap_or_else(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
    if !root.is_mapping() {
        root = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
    }
    let Some(root_map) = root.as_mapping_mut() else {
        return Err("failed to initialize models/schema.yml root mapping".to_string());
    };
    root_map
        .entry(serde_yaml::Value::String("version".to_string()))
        .or_insert_with(|| serde_yaml::Value::Number(2.into()));
    let models_key = serde_yaml::Value::String("models".to_string());
    if !root_map.contains_key(&models_key) {
        root_map.insert(models_key.clone(), serde_yaml::Value::Sequence(Vec::new()));
    }
    let Some(sequence) = root_map
        .get_mut(&models_key)
        .and_then(|value| value.as_sequence_mut())
    else {
        return Err("models/schema.yml top-level models key must be a sequence".to_string());
    };
    sequence.retain(|value| {
        value
            .as_mapping()
            .and_then(|mapping| yaml_str(mapping, "name"))
            .map(|name| !touched.contains(name.trim()))
            .unwrap_or(true)
    });
    for model in models {
        sequence.push(schema_model_value(model)?);
    }
    serde_yaml::to_string(&root)
        .map(escape_yaml_doc_preamble)
        .map_err(|e| format!("failed to render models/schema.yml: {e}"))
}

fn validation_patch_outcome(
    ctx: &AgentCtx,
    rel: &str,
    old: &str,
    new: &str,
    existed: bool,
) -> Result<project_fs::PatchOutcome, String> {
    Ok(project_fs::PatchOutcome {
        rel_path: rel.to_string(),
        key: project_fs::join_storage_key(ctx, rel),
        existed,
        base_sha256: project_fs::sha256_hex(old),
        new_sha256: project_fs::sha256_hex(new),
        git_patch: project_fs::create_git_patch_text(old, new, rel, existed)?,
        diff: String::new(),
        lines_added: 0,
        lines_removed: 0,
        content: new.to_string(),
        apply_result_code: project_fs::PatchApplyResultCode::AppliedUnifiedDirect,
        apply_repairs: Vec::new(),
    })
}

use super::model_authoring_engine::extract_string_arg;

#[derive(Clone)]
pub struct ApplyNextCleanseSchemaBatchTool {
    pub datasets: Option<Arc<dyn DatasetCatalogProvider>>,
}

#[async_trait]
impl Tool for ApplyNextCleanseSchemaBatchTool {
    fn name(&self) -> &'static str {
        "apply_next_cleanse_schema_batch"
    }

    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
        let checklist_item_id = plan::schema_contract_checklist_item_id().to_string();

        let mut plan = plan::load_cleanse_plan(ctx)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "no active cleanse plan found".to_string())?;
        if plan.status != plan::PlanStatus::Approved && plan.status != plan::PlanStatus::Completed {
            return Err(format!(
                "cleanse plan is not approved (status={:?}); return to plan phase",
                plan.status
            ));
        }

        let v = crate::plan_semantic_gate::gate_cleanse_plan(&mut plan).into_validation();
        if !v.ok {
            return crate::tools::batch_contracts::to_json_value(
                crate::tools::batch_contracts::CleanseSchemaBatchContract {
                    ok: false,
                    kind: Some("plan_invalid".to_string()),
                    reason_code: None,
                    message: Some(format!("plan_key={}", plan.plan_key)),
                    checklist_item_id: checklist_item_id.clone(),
                    attempted_dataset_ids: Vec::new(),
                    succeeded_dataset_ids: Vec::new(),
                    failed_dataset_ids: Vec::new(),
                    errors: v.errors,
                    progress_made: None,
                    auto_healed_wildcard_sql_dataset_ids: Vec::new(),
                    plan_violations: Vec::new(),
                },
            );
        }

        if controller_kernel::batch_budget(&plan.progress).exhausted() {
            return crate::tools::batch_contracts::to_json_value(
                crate::tools::batch_contracts::CleanseSchemaBatchContract {
                    ok: false,
                    kind: Some("batch_locked".to_string()),
                    reason_code: Some(
                        serde_json::to_value(
                            controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted,
                        )
                        .map_err(|e| format!("failed to encode batch lock reason: {e}"))?,
                    ),
                    message: Some(
                        controller_kernel::batch_lock_error_message(
                            controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted,
                        )
                        .to_string(),
                    ),
                    checklist_item_id: checklist_item_id.clone(),
                    attempted_dataset_ids: Vec::new(),
                    succeeded_dataset_ids: Vec::new(),
                    failed_dataset_ids: Vec::new(),
                    errors: vec![controller_kernel::batch_lock_error_message(
                        controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted,
                    )
                    .to_string()],
                    progress_made: None,
                    auto_healed_wildcard_sql_dataset_ids: Vec::new(),
                    plan_violations: Vec::new(),
                },
            );
        }

        let batch = plan::cleanse_pending_schema_contracts(&plan);
        if batch.is_empty() {
            let has_incomplete = plan
                .tasks
                .iter()
                .any(|t| !matches!(t.status, crate::plan_types::TaskStatus::Done));
            return crate::tools::batch_contracts::to_json_value(
                crate::tools::batch_contracts::CleanseSchemaBatchContract {
                    ok: !has_incomplete,
                    kind: None,
                    reason_code: None,
                    message: Some(if has_incomplete {
                        "schema batch has no pending work but plan tasks are incomplete; SQL authoring must complete first".to_string()
                    } else {
                        "all schema work complete".to_string()
                    }),
                    checklist_item_id: checklist_item_id.clone(),
                    attempted_dataset_ids: Vec::new(),
                    succeeded_dataset_ids: Vec::new(),
                    failed_dataset_ids: Vec::new(),
                    errors: Vec::new(),
                    progress_made: Some(false),
                    auto_healed_wildcard_sql_dataset_ids: Vec::new(),
                    plan_violations: Vec::new(),
                },
            );
        }
        if let Err(e) = chunk_progress_contract::enforce_chunk_contract(
            &batch,
            crate::plan_progress::MAX_BATCH_SIZE,
            "cleanse_schema",
        ) {
            return crate::tools::batch_contracts::to_json_value(
                crate::tools::batch_contracts::CleanseSchemaBatchContract {
                    ok: false,
                    kind: Some("chunk_contract_violation".to_string()),
                    reason_code: None,
                    message: None,
                    checklist_item_id: checklist_item_id.clone(),
                    attempted_dataset_ids: Vec::new(),
                    succeeded_dataset_ids: Vec::new(),
                    failed_dataset_ids: Vec::new(),
                    errors: vec![e],
                    progress_made: None,
                    auto_healed_wildcard_sql_dataset_ids: Vec::new(),
                    plan_violations: Vec::new(),
                },
            );
        }

        let _instructions = extract_string_arg(&args, "instructions")
            .or_else(|| extract_string_arg(&args, "user_instructions"))
            .unwrap_or_default();

        for ds in batch.iter() {
            plan::cleanse_schema_contract_mark_in_progress(&mut plan, ds);
        }
        plan::save_cleanse_plan(ctx, &plan)
            .await
            .map_err(|e| format!("failed to persist cleanse schema batch start state: {e}"))?;

        tracing::info!(
            plan_key = %plan.plan_key,
            datasets = ?batch,
            "apply_next_cleanse_schema_batch: authoring models/staging/*.yml (silver schema) per dataset"
        );

        let mut succeeded: Vec<String> = Vec::new();
        let mut failed: Vec<String> = Vec::new();
        let mut errors: Vec<String> = Vec::new();
        let auto_healed_wildcard_sql_dataset_ids: Vec<String> = Vec::new();
        let mut plan_violations: Vec<crate::tools::batch_contracts::PlanViolationBrief> =
            Vec::new();

        for ds in batch.iter() {
            let Some(ds_ref) = DatasetRef::parse(ds) else {
                failed.push(ds.clone());
                errors.push(format!(
                    "{ds}: invalid dataset_id (expected <catalog>.<schema>.<table>)"
                ));
                continue;
            };
            let schema = ds_ref.schema;
            let table = ds_ref.table;

            let model_name = naming::canonical_staging_model_name(&schema, &table);
            let sql_rel = naming::canonical_staging_rel_path(&schema, &table);
            let yml_rel = format!("models/staging/{}.yml", model_name);

            let sql_text = match project_fs::read_project_file_text(ctx, &sql_rel).await {
                Ok(Some(text)) => text,
                Ok(None) | Err(_) => {
                    failed.push(ds.clone());
                    errors.push(format!("{ds}: missing sibling SQL {sql_rel}"));
                    continue;
                }
            };
            let allowed_cols = match files_tool::extract_final_select_output_columns(&sql_text) {
                Ok(s) => s.into_iter().collect::<Vec<_>>(),
                Err(e) => {
                    failed.push(ds.clone());
                    let msg = format!(
                        "{ds}: cannot determine output columns from sibling SQL {sql_rel}; schema YAML is generated from SQL output columns only ({e})"
                    );
                    errors.push(msg.clone());
                    plan_violations.push(crate::tools::batch_contracts::PlanViolationBrief {
                        task_id: ds.clone(),
                        evidence: msg,
                    });
                    continue;
                }
            };
            let mut allowed_cols = allowed_cols;
            allowed_cols.sort();
            allowed_cols.dedup();

            let existing_yml = project_fs::read_project_file_text(ctx, &yml_rel)
                .await
                .unwrap_or_default();
            let planned_fields = plan
                .tasks
                .iter()
                .find(|t| t.dataset_id == *ds)
                .and_then(|task| task.implementation_spec.as_ref())
                .map(|spec| spec.output_fields.as_slice())
                .unwrap_or(&[]);
            let rendered = match render_staging_schema_yml(
                &model_name,
                ds,
                &allowed_cols,
                existing_yml.as_deref(),
                planned_fields,
            ) {
                Ok(rendered) => rendered,
                Err(e) => {
                    failed.push(ds.clone());
                    errors.push(format!("{ds}: schema generation failed: {e}"));
                    continue;
                }
            };
            let old = existing_yml.clone().unwrap_or_default();
            let outcome = match validation_patch_outcome(
                ctx,
                &yml_rel,
                &old,
                &rendered,
                existing_yml.is_some(),
            ) {
                Ok(outcome) => outcome,
                Err(e) => {
                    failed.push(ds.clone());
                    errors.push(format!("{ds}: schema generation failed: {e}"));
                    continue;
                }
            };

            // Validate schema contract against sibling SQL output columns.
            if let Err(e) =
                files_tool::validate_staging_schema_ymls(ctx, std::slice::from_ref(&outcome)).await
            {
                failed.push(ds.clone());
                errors.push(format!("{ds}: invalid staging schema yml: {e}"));
                continue;
            }

            if let Err(e) =
                project_fs::write_file(ctx, self.datasets.as_ref(), &yml_rel, &outcome.content)
                    .await
            {
                failed.push(ds.clone());
                errors.push(format!("{ds}: failed to write {yml_rel}: {e}"));
                continue;
            }

            succeeded.push(ds.clone());
        }

        for ds in succeeded.iter() {
            plan::cleanse_schema_contract_mark_done(&mut plan, ds);
        }
        for ds in failed.iter() {
            plan::cleanse_schema_contract_mark_needs_update(
                &mut plan,
                ds,
                Some(errors.join("\n").as_str()),
            );
        }
        let failure_kind = if failed.is_empty() {
            None
        } else {
            Some(
                crate::tools::batch_sql_runner::classify_schema_batch_failure_kind(
                    &errors.join("\n"),
                ),
            )
        };
        let budget = controller_kernel::note_batch_result_with_failure_kind(
            &mut plan.progress,
            failed.is_empty(),
            failure_kind,
        );
        plan::save_cleanse_plan(ctx, &plan)
            .await
            .map_err(|e| format!("failed to persist cleanse schema batch result state: {e}"))?;

        if budget.exhausted() {
            return crate::tools::batch_contracts::to_json_value(
                crate::tools::batch_contracts::CleanseSchemaBatchContract {
                    ok: false,
                    kind: Some("batch_locked".to_string()),
                    reason_code: Some(
                        serde_json::to_value(
                            controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted,
                        )
                        .map_err(|e| format!("failed to encode batch lock reason: {e}"))?,
                    ),
                    message: Some(
                        controller_kernel::batch_lock_error_message(
                            controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted,
                        )
                        .to_string(),
                    ),
                    checklist_item_id: checklist_item_id.clone(),
                    attempted_dataset_ids: batch,
                    succeeded_dataset_ids: succeeded,
                    failed_dataset_ids: failed,
                    auto_healed_wildcard_sql_dataset_ids,
                    errors,
                    progress_made: None,
                    plan_violations,
                },
            );
        }

        crate::tools::batch_contracts::to_json_value(
            crate::tools::batch_contracts::CleanseSchemaBatchContract {
                ok: failed.is_empty(),
                kind: None,
                reason_code: None,
                message: None,
                checklist_item_id,
                attempted_dataset_ids: batch,
                succeeded_dataset_ids: succeeded.clone(),
                failed_dataset_ids: failed,
                auto_healed_wildcard_sql_dataset_ids,
                errors,
                progress_made: Some(!succeeded.is_empty()),
                plan_violations,
            },
        )
    }
}

#[derive(Clone)]
pub struct ApplyNextModelSchemaBatchTool {
    pub datasets: Option<Arc<dyn DatasetCatalogProvider>>,
}

#[async_trait]
impl Tool for ApplyNextModelSchemaBatchTool {
    fn name(&self) -> &'static str {
        "apply_next_model_schema_batch"
    }

    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
        let checklist_item_id = plan::schema_contract_checklist_item_id().to_string();

        let mut plan = plan::load_model_plan(ctx)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "no active model plan found".to_string())?;
        if plan.status != plan::PlanStatus::Approved && plan.status != plan::PlanStatus::Completed {
            return Err(format!(
                "model plan is not approved (status={:?}); return to plan phase",
                plan.status
            ));
        }

        let stg = crate::dataset_truth::discover_staging_models_from_storage(ctx).await;
        let v = crate::plan_semantic_gate::gate_model_plan(&mut plan, &stg.allowed_models)
            .into_validation();
        if !v.ok {
            return crate::tools::batch_contracts::to_json_value(
                crate::tools::batch_contracts::ModelSchemaBatchContract {
                    ok: false,
                    checklist_item_id: checklist_item_id.clone(),
                    kind: Some("plan_invalid".to_string()),
                    reason_code: None,
                    message: Some(format!("plan_key={}", plan.plan_key)),
                    errors: v.errors,
                    attempted_item_names: Vec::new(),
                    succeeded_item_names: Vec::new(),
                    failed_item_names: Vec::new(),
                    progress_made: None,
                    warnings: Vec::new(),
                    plan_violations: Vec::new(),
                },
            );
        }

        if controller_kernel::batch_budget(&plan.progress).exhausted() {
            return crate::tools::batch_contracts::to_json_value(
                crate::tools::batch_contracts::ModelSchemaBatchContract {
                    ok: false,
                    checklist_item_id: checklist_item_id.clone(),
                    kind: Some("batch_locked".to_string()),
                    reason_code: Some(
                        serde_json::to_value(
                            controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted,
                        )
                        .map_err(|e| format!("failed to encode batch lock reason: {e}"))?,
                    ),
                    message: Some(
                        controller_kernel::batch_lock_error_message(
                            controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted,
                        )
                        .to_string(),
                    ),
                    errors: vec![controller_kernel::batch_lock_error_message(
                        controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted,
                    )
                    .to_string()],
                    attempted_item_names: Vec::new(),
                    succeeded_item_names: Vec::new(),
                    failed_item_names: Vec::new(),
                    progress_made: None,
                    warnings: Vec::new(),
                    plan_violations: Vec::new(),
                },
            );
        }

        let names = plan::model_pending_schema_contracts(&plan);
        if names.is_empty() {
            let has_incomplete = plan
                .tasks
                .iter()
                .any(|t| !matches!(t.status, crate::plan_types::TaskStatus::Done));
            return crate::tools::batch_contracts::to_json_value(
                crate::tools::batch_contracts::ModelSchemaBatchContract {
                    ok: !has_incomplete,
                    checklist_item_id: checklist_item_id.clone(),
                    kind: None,
                    reason_code: None,
                    message: Some(if has_incomplete {
                        "schema batch has no pending work but plan tasks are incomplete; SQL authoring must complete first".to_string()
                    } else {
                        "all schema work complete".to_string()
                    }),
                    errors: Vec::new(),
                    attempted_item_names: Vec::new(),
                    succeeded_item_names: Vec::new(),
                    failed_item_names: Vec::new(),
                    progress_made: Some(false),
                    warnings: Vec::new(),
                    plan_violations: Vec::new(),
                },
            );
        }
        if let Err(e) = chunk_progress_contract::enforce_chunk_contract(
            &names,
            crate::plan_progress::MAX_BATCH_SIZE,
            "model_schema",
        ) {
            return crate::tools::batch_contracts::to_json_value(
                crate::tools::batch_contracts::ModelSchemaBatchContract {
                    ok: false,
                    checklist_item_id: checklist_item_id.clone(),
                    kind: Some("chunk_contract_violation".to_string()),
                    reason_code: None,
                    message: None,
                    errors: vec![e],
                    attempted_item_names: Vec::new(),
                    succeeded_item_names: Vec::new(),
                    failed_item_names: Vec::new(),
                    progress_made: None,
                    warnings: Vec::new(),
                    plan_violations: Vec::new(),
                },
            );
        }

        let _instructions = extract_string_arg(&args, "instructions")
            .or_else(|| extract_string_arg(&args, "user_instructions"))
            .unwrap_or_default();

        for n in names.iter() {
            plan::model_schema_contract_mark_in_progress(&mut plan, n);
        }
        plan::save_model_plan(ctx, &plan)
            .await
            .map_err(|e| format!("failed to persist model schema batch start state: {e}"))?;

        let attempted_names = names.clone();
        let expected_rel = project_fs::MODELS_SCHEMA_YML;

        let existing_models_schema = project_fs::read_project_file_text(ctx, expected_rel)
            .await
            .unwrap_or_default();
        let mut generated_models: Vec<SchemaModelDoc> = Vec::new();
        let mut generation_errors = Vec::new();
        for n in names.iter() {
            if let Some(t) = plan.tasks.iter().find(|t| t.name == *n) {
                // Derive allowed columns from the model SQL output projection (best-effort).
                let columns = if let Some(ref rel) = t.expected_model_path {
                    match project_fs::read_project_file_text(ctx, rel).await {
                        Ok(Some(sql_text)) => {
                            match files_tool::extract_final_select_output_columns(&sql_text) {
                                Ok(cols) => cols.into_iter().collect::<Vec<_>>(),
                                Err(e) => {
                                    generation_errors.push(format!(
                                        "{}: cannot determine output columns from {} ({})",
                                        t.name, rel, e
                                    ));
                                    Vec::new()
                                }
                            }
                        }
                        Ok(None) => {
                            generation_errors
                                .push(format!("{}: missing model SQL at {rel}", t.name));
                            Vec::new()
                        }
                        Err(e) => {
                            generation_errors.push(format!(
                                "{}: failed to read model SQL at {rel}: {e}",
                                t.name
                            ));
                            Vec::new()
                        }
                    }
                } else {
                    generation_errors.push(format!(
                        "{}: expected_model_path missing in plan task",
                        t.name
                    ));
                    Vec::new()
                };
                if columns.is_empty() {
                    continue;
                }
                let existing = existing_model_meta(existing_models_schema.as_deref(), &t.name);
                let planned = t
                    .implementation_spec
                    .as_ref()
                    .map(|spec| output_field_meta(&spec.output_fields))
                    .unwrap_or_default();
                generated_models.push(schema_model_doc(
                    &t.name,
                    (!t.goal.trim().is_empty()).then(|| t.goal.trim().to_string()),
                    &columns,
                    &existing,
                    &planned,
                    false,
                ));
            } else {
                generation_errors.push(format!("{n}: model task missing from active plan"));
            }
        }
        if !generation_errors.is_empty() {
            return crate::tools::batch_schema_runner::fail_model_schema_batch(
                ctx,
                &mut plan,
                &attempted_names,
                &checklist_item_id,
                generation_errors.join("\n"),
            )
            .await;
        }

        let sanitized_text =
            match merge_models_schema_yml(existing_models_schema.as_deref(), generated_models) {
                Ok(text) => text,
                Err(e) => {
                    return crate::tools::batch_schema_runner::fail_model_schema_batch(
                        ctx,
                        &mut plan,
                        &attempted_names,
                        &checklist_item_id,
                        format!("models/schema.yml generation failed: {e}"),
                    )
                    .await;
                }
            };
        let warnings = Vec::new();

        if let Err(e) =
            project_fs::write_file(ctx, self.datasets.as_ref(), expected_rel, &sanitized_text).await
        {
            return crate::tools::batch_schema_runner::fail_model_schema_batch(
                ctx,
                &mut plan,
                &attempted_names,
                &checklist_item_id,
                format!("failed to write {}: {}", expected_rel, e),
            )
            .await;
        }

        for n in names.iter() {
            plan::model_schema_contract_mark_done(&mut plan, n);
        }
        let budget =
            controller_kernel::note_batch_result_with_failure_kind(&mut plan.progress, true, None);
        plan::save_model_plan(ctx, &plan)
            .await
            .map_err(|e| format!("failed to persist model schema batch result state: {e}"))?;

        if budget.exhausted() {
            return crate::tools::batch_contracts::to_json_value(
                crate::tools::batch_contracts::ModelSchemaBatchContract {
                    ok: false,
                    checklist_item_id: checklist_item_id.clone(),
                    kind: Some("batch_locked".to_string()),
                    reason_code: Some(
                        serde_json::to_value(
                            controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted,
                        )
                        .map_err(|e| format!("failed to encode batch lock reason: {e}"))?,
                    ),
                    message: Some(
                        controller_kernel::batch_lock_error_message(
                            controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted,
                        )
                        .to_string(),
                    ),
                    attempted_item_names: attempted_names.clone(),
                    succeeded_item_names: attempted_names.clone(),
                    failed_item_names: Vec::new(),
                    errors: vec![controller_kernel::batch_lock_error_message(
                        controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted,
                    )
                    .to_string()],
                    progress_made: None,
                    warnings,
                    plan_violations: Vec::new(),
                },
            );
        }

        crate::tools::batch_contracts::to_json_value(
            crate::tools::batch_contracts::ModelSchemaBatchContract {
                ok: true,
                checklist_item_id,
                kind: None,
                reason_code: None,
                message: None,
                attempted_item_names: attempted_names.clone(),
                succeeded_item_names: attempted_names,
                failed_item_names: Vec::new(),
                errors: Vec::new(),
                progress_made: Some(true),
                warnings,
                plan_violations: Vec::new(),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx_ext::{ProvidersCfgCap, WarehouseCap};
    use crate::de_config;
    use crate::providers::warehouse::NullWarehouseProvider;
    use crate::track_spec::TrackKind;
    use react_core::keyspace::{DefaultKeyspace, Keyspace};
    use react_core::llm::ChatMessage;
    use react_core::llm::LargeLanguageModel;
    use react_core::scope::RequestScope;
    use react_core::session::ExecutionContext;
    use react_core::storage::StorageAdapter;
    use react_module_storage_memory::InMemoryStorageAdapter;
    use std::sync::Arc;
    use std::sync::Mutex;

    #[derive(Default)]
    struct ScriptedLlm {
        replies: Mutex<Vec<String>>,
    }

    impl LargeLanguageModel for ScriptedLlm {
        fn chat(
            &self,
            _messages: &[ChatMessage],
            _options: &react_core::llm::LlmCallOptions,
        ) -> Result<String, String> {
            let mut g = self
                .replies
                .lock()
                .map_err(|_| "mutex poisoned".to_string())?;
            if g.is_empty() {
                return Err("no more replies".to_string());
            }
            Ok(g.remove(0))
        }
        fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
            Ok(vec![])
        }
    }

    fn observed_customer_key_claim() -> Vec<crate::providers::SemanticClaimRef> {
        vec![crate::providers::SemanticClaimRef {
            claim_id: "candidate_key:test_raw.raw_customers:customer_id"
                .to_string()
                .into(),
            kind: crate::providers::SemanticClaimKind::CandidateKey,
            status: crate::providers::EvidenceStatus::Observed,
        }]
    }

    struct InspectingLlm {
        reply: String,
        saw_allowed_columns: Mutex<bool>,
    }

    impl LargeLanguageModel for InspectingLlm {
        fn chat(
            &self,
            messages: &[ChatMessage],
            _options: &react_core::llm::LlmCallOptions,
        ) -> Result<String, String> {
            // Find the last user message (patch_protocol sends JSON payload as user content).
            let user = messages
                .iter()
                .rev()
                .find(|m| m.role == react_core::llm::ChatRole::User)
                .map(|m| m.content.clone())
                .unwrap_or_default();
            let v: serde_json::Value = serde_json::from_str(&user)
                .map_err(|e| format!("expected JSON user payload: {e}"))?;
            let allowed = v
                .get("input")
                .and_then(|x| x.get("models"))
                .and_then(|x| x.as_array())
                .and_then(|a| a.first())
                .and_then(|m| m.get("allowed_columns"))
                .and_then(|x| x.as_array())
                .cloned()
                .unwrap_or_default();
            let allowed_strs: Vec<String> = allowed
                .into_iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect();
            if allowed_strs.contains(&"customer_id".to_string())
                && allowed_strs.contains(&"email".to_string())
            {
                let mut g = self
                    .saw_allowed_columns
                    .lock()
                    .map_err(|_| "mutex poisoned".to_string())?;
                *g = true;
            } else {
                return Err(format!(
                    "allowed_columns missing expected items: {:?}",
                    allowed_strs
                ));
            }
            Ok(self.reply.clone())
        }
        fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
            Ok(vec![])
        }
    }

    fn minimal_cfg() -> Arc<react_core::resolved_config::ReactResolvedConfig> {
        Arc::new(react_core::resolved_config::ReactResolvedConfig {
            server: react_core::resolved_config::ServerResolved { port: 1 },
            storage: react_core::resolved_config::StorageResolved {
                mode: react_core::resolved_config::StorageMode::Local,
                bucket: None,
                path: None,
                s3_credentials: None,
            },
            scope: RequestScope::parse("t", "w", "p").expect("valid test scope"),
            llm: react_core::resolved_config::LlmResolved::default(),
            suite_config: serde_json::json!({
                "warehouse": {
                    "kind": "athena",
                    "container": "AwsDataCatalog",
                    "namespace": "test_raw",
                    "extras": {"region":"eu-west-1","workgroup":"wg","result_s3":"s3://x/"}
                },
                "catalog": {
                    "enabled": false,
                    "refresh_secs": 60,
                    "max_concurrency": 8
                },
                "dbt": {
                    "enabled": true,
                    "target": "athena",
                    "naming": {
                        "target_schema": "test",
                        "silver_suffix": "silver",
                        "gold_suffix": "gold"
                    },
                    "runner": "host"
                },
                "vector": {
                    "enabled": false
                }
            }),
        })
    }

    #[tokio::test]
    async fn apply_next_cleanse_schema_batch_writes_canonical_staging_yml() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let llm: Arc<dyn LargeLanguageModel> = Arc::new(ScriptedLlm {
            replies: Mutex::new(vec![serde_json::json!({
                "path": "models/staging/stg_test_raw_raw_customers.yml",
                "patch_text": "@@ ... @@\n+version: 2\n+\n+models:\n+  - name: stg_test_raw_raw_customers\n+    columns:\n+      - name: customer_id_raw\n+      - name: email_raw\n"
            }).to_string()]),
        });
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        let scope = RequestScope::parse("t", "w", "p").expect("valid test scope");
        let mut ctx = react_core::agent::AgentCtxBuilder::new(
            llm,
            storage.clone(),
            scope.clone(),
            keyspace,
            Arc::new(react_core::agent::DefaultPolicy),
        )
        .top_k(1)
        .per_step_timeout_secs(1)
        .max_steps(2)
        .thread_id("tid".to_string())
        .agent_name("test".to_string())
        .resolved_config(Some(minimal_cfg()))
        .build();
        let providers =
            de_config::de_config_from_resolved(ctx.resolved_config().as_ref().unwrap()).unwrap();
        ctx.set_capability(Arc::new(ProvidersCfgCap(providers)));
        ctx.set_capability(Arc::new(WarehouseCap(
            Arc::new(NullWarehouseProvider) as Arc<dyn crate::providers::WarehouseProvider>
        )));

        // Seed a cleanse plan with sql_model done and schema_contract pending.
        let plan_key = plan::new_cleanse_plan_key(&ctx);
        let mut checklist = plan::canonical_task_checklist(TrackKind::Cleanse);
        if let Some(item) = checklist
            .iter_mut()
            .find(|it| it.checklist_item_id == plan::CHECKLIST_SQL_MODEL)
        {
            item.status = plan::ChecklistItemStatus::Done;
        }
        let batches = vec![vec!["AwsDataCatalog.test_raw.raw_customers".to_string()]];
        let p = plan::CleansePlan {
            plan_key: plan_key.clone(),
            status: plan::PlanStatus::Approved,
            project_snapshot: Default::default(),
            tasks: vec![plan::CleanseTask {
                dataset_id: "AwsDataCatalog.test_raw.raw_customers".to_string(),
                expected_model_path: Some(
                    "models/staging/stg_test_raw_raw_customers.sql".to_string(),
                ),
                invariants: vec![],
                implementation_spec: Some(plan::CleanseImplementationSpec {
                    spec_version: 1,
                    row_preserving: true,
                    output_fields: vec![
                        plan::OutputFieldSpec {
                            name: "customer_id_raw".to_string(),
                            kind: plan::FieldKind::Raw,
                            lineage: vec![plan::FieldLineage::column(
                                plan::SourceFieldRef {
                                    relation: None,
                                    name: "customer_id".to_string(),
                                },
                                plan::LineageRole::Passthrough,
                            )],
                            expression: "customer_id as customer_id_raw (raw)".to_string(),
                            data_type: None,
                            nullable: true,
                            description: None,
                        },
                        plan::OutputFieldSpec {
                            name: "email_raw".to_string(),
                            kind: plan::FieldKind::Raw,
                            lineage: vec![plan::FieldLineage::column(
                                plan::SourceFieldRef {
                                    relation: None,
                                    name: "email".to_string(),
                                },
                                plan::LineageRole::Passthrough,
                            )],
                            expression: "email as email_raw (raw)".to_string(),
                            data_type: None,
                            nullable: true,
                            description: None,
                        },
                    ],
                    prohibited_ops: vec![],
                }),
                source_schema: vec![],
                status: plan::TaskStatus::InProgress,
                checklist,
            }],
            batches: batches.clone(),
            work_groups: plan::canonical_work_groups_from_batches(&batches, "cleanse"),
            mutations: vec![],
            progress: plan::PlanProgress::default(),
        };
        plan::save_cleanse_plan(&ctx, &p).await.unwrap();

        // Seed staging SQL with explicit output columns; schema YAML is generated from this SQL.
        let sql_rel = "models/staging/stg_test_raw_raw_customers.sql";
        let sql_key = project_fs::join_storage_key(&ctx, sql_rel);
        let sql = "select\n  customer_id_raw,\n  email_raw\nfrom {{ source('test_raw','raw_customers') }}\n";
        ctx.storage()
            .put_bytes(&sql_key, sql.as_bytes(), "text/sql")
            .await
            .unwrap();

        let tool = ApplyNextCleanseSchemaBatchTool { datasets: None };
        let res = tool.call(serde_json::json!({}), &ctx).await.unwrap();
        assert!(
            res.get("ok").and_then(|v| v.as_bool()).unwrap_or(false),
            "unexpected tool response: {}",
            res
        );

        let yml_rel = "models/staging/stg_test_raw_raw_customers.yml";
        let yml_key = project_fs::join_storage_key(&ctx, yml_rel);
        let got = ctx.storage().get_bytes(&yml_key).await.unwrap();
        let got = String::from_utf8_lossy(&got).to_string();
        assert!(got.contains("stg_test_raw_raw_customers"));

        assert!(
            got.contains("customer_id_raw") && got.contains("email_raw"),
            "expected generated YAML to use SQL output columns, got: {}",
            got
        );
        let healed_ds = res
            .get("auto_healed_wildcard_sql_dataset_ids")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        assert!(healed_ds.is_empty());
    }

    #[tokio::test]
    async fn apply_next_cleanse_schema_batch_stops_when_local_failure_budget_exhausted() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let llm: Arc<dyn LargeLanguageModel> = Arc::new(ScriptedLlm {
            replies: Mutex::new(vec![]),
        });
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        let scope = RequestScope::parse("t", "w", "p").expect("valid test scope");
        let mut ctx = react_core::agent::AgentCtxBuilder::new(
            llm,
            storage.clone(),
            scope.clone(),
            keyspace,
            Arc::new(react_core::agent::DefaultPolicy),
        )
        .top_k(1)
        .per_step_timeout_secs(1)
        .max_steps(2)
        .thread_id("tid_lock".to_string())
        .agent_name("test".to_string())
        .resolved_config(Some(minimal_cfg()))
        .build();
        let providers =
            de_config::de_config_from_resolved(ctx.resolved_config().as_ref().unwrap()).unwrap();
        ctx.set_capability(Arc::new(ProvidersCfgCap(providers)));
        ctx.set_capability(Arc::new(WarehouseCap(
            Arc::new(NullWarehouseProvider) as Arc<dyn crate::providers::WarehouseProvider>
        )));

        let plan_key = plan::new_cleanse_plan_key(&ctx);
        let mut checklist = plan::canonical_task_checklist(TrackKind::Cleanse);
        if let Some(item) = checklist
            .iter_mut()
            .find(|it| it.checklist_item_id == plan::CHECKLIST_SQL_MODEL)
        {
            item.status = plan::ChecklistItemStatus::Done;
        }
        let batches = vec![vec!["AwsDataCatalog.test_raw.raw_customers".to_string()]];
        let mut progress = plan::PlanProgress::default();
        progress.consecutive_batch_failures = controller_kernel::max_consecutive_batch_failures();
        let p = plan::CleansePlan {
            plan_key,
            status: plan::PlanStatus::Approved,
            project_snapshot: Default::default(),
            tasks: vec![plan::CleanseTask {
                dataset_id: "AwsDataCatalog.test_raw.raw_customers".to_string(),
                expected_model_path: Some(
                    "models/staging/stg_test_raw_raw_customers.sql".to_string(),
                ),
                invariants: vec![],
                implementation_spec: Some(plan::CleanseImplementationSpec {
                    spec_version: 1,
                    row_preserving: true,
                    output_fields: vec![plan::OutputFieldSpec {
                        name: "customer_id_raw".to_string(),
                        kind: plan::FieldKind::Raw,
                        lineage: vec![plan::FieldLineage::column(
                            plan::SourceFieldRef {
                                relation: None,
                                name: "customer_id".to_string(),
                            },
                            plan::LineageRole::Passthrough,
                        )],
                        expression: "customer_id as customer_id_raw (raw)".to_string(),
                        data_type: None,
                        nullable: true,
                        description: None,
                    }],
                    prohibited_ops: vec![],
                }),
                source_schema: vec![],
                status: plan::TaskStatus::InProgress,
                checklist,
            }],
            batches: batches.clone(),
            work_groups: plan::canonical_work_groups_from_batches(&batches, "cleanse"),
            mutations: vec![],
            progress,
        };
        plan::save_cleanse_plan(&ctx, &p).await.unwrap();

        let tool = ApplyNextCleanseSchemaBatchTool { datasets: None };
        let res = tool.call(serde_json::json!({}), &ctx).await.unwrap();
        assert_eq!(
            res.get("kind").and_then(|v| v.as_str()),
            Some("batch_locked")
        );
        assert_eq!(
            res.get("attempted_dataset_ids")
                .and_then(|v| v.as_array())
                .map(|a| a.len()),
            Some(0)
        );
    }

    #[tokio::test]
    async fn apply_next_cleanse_schema_batch_ignores_exec_ctx_checklist_override() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let llm: Arc<dyn LargeLanguageModel> = Arc::new(ScriptedLlm {
            replies: Mutex::new(vec![serde_json::json!({
                "path": "models/staging/stg_test_raw_raw_customers.yml",
                "patch_text": "@@ ... @@\n+version: 2\n+\n+models:\n+  - name: stg_test_raw_raw_customers\n+    columns:\n+      - name: customer_id_raw\n+      - name: email_raw\n"
            })
            .to_string()]),
        });
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        let scope = RequestScope::parse("t", "w", "p").expect("valid test scope");
        let mut ctx = react_core::agent::AgentCtxBuilder::new(
            llm,
            storage.clone(),
            scope.clone(),
            keyspace,
            Arc::new(react_core::agent::DefaultPolicy),
        )
        .top_k(1)
        .per_step_timeout_secs(1)
        .max_steps(2)
        .thread_id("tid_ctx".to_string())
        .agent_name("test".to_string())
        .resolved_config(Some(minimal_cfg()))
        .build();
        let providers =
            de_config::de_config_from_resolved(ctx.resolved_config().as_ref().unwrap()).unwrap();
        ctx.set_capability(Arc::new(ProvidersCfgCap(providers)));
        ctx.set_capability(Arc::new(WarehouseCap(
            Arc::new(NullWarehouseProvider) as Arc<dyn crate::providers::WarehouseProvider>
        )));

        let plan_key = plan::new_cleanse_plan_key(&ctx);
        ctx.set_exec_ctx(Some({
            let mut ectx = ExecutionContext::default();
            ectx.set(
                "plan_kind",
                serde_json::Value::String("cleanse".to_string()),
            );
            ectx.set("plan_key", serde_json::Value::String(plan_key.clone()));
            ectx.set("workgroup_id", serde_json::Value::String("wg".to_string()));
            ectx.set(
                "task_id",
                serde_json::Value::String("AwsDataCatalog.test_raw.raw_customers".to_string()),
            );
            ectx.set(
                "checklist_item_id",
                serde_json::Value::String(plan::CHECKLIST_SQL_MODEL.to_string()),
            );
            ectx
        }));

        let mut checklist = plan::canonical_task_checklist(TrackKind::Cleanse);
        if let Some(item) = checklist
            .iter_mut()
            .find(|it| it.checklist_item_id == plan::CHECKLIST_SQL_MODEL)
        {
            item.status = plan::ChecklistItemStatus::Done;
        }
        let batches = vec![vec!["AwsDataCatalog.test_raw.raw_customers".to_string()]];
        let p = plan::CleansePlan {
            plan_key: plan_key.clone(),
            status: plan::PlanStatus::Approved,
            project_snapshot: Default::default(),
            tasks: vec![plan::CleanseTask {
                dataset_id: "AwsDataCatalog.test_raw.raw_customers".to_string(),
                expected_model_path: Some(
                    "models/staging/stg_test_raw_raw_customers.sql".to_string(),
                ),
                invariants: vec![],
                implementation_spec: Some(plan::CleanseImplementationSpec {
                    spec_version: 1,
                    row_preserving: true,
                    output_fields: vec![plan::OutputFieldSpec {
                        name: "customer_id_raw".to_string(),
                        kind: plan::FieldKind::Raw,
                        lineage: vec![plan::FieldLineage::column(
                            plan::SourceFieldRef {
                                relation: None,
                                name: "customer_id".to_string(),
                            },
                            plan::LineageRole::Passthrough,
                        )],
                        expression: "customer_id as customer_id_raw (raw)".to_string(),
                        data_type: None,
                        nullable: true,
                        description: None,
                    }],
                    prohibited_ops: vec![],
                }),
                source_schema: vec![],
                status: plan::TaskStatus::InProgress,
                checklist,
            }],
            batches: batches.clone(),
            work_groups: plan::canonical_work_groups_from_batches(&batches, "cleanse"),
            mutations: vec![],
            progress: plan::PlanProgress::default(),
        };
        plan::save_cleanse_plan(&ctx, &p).await.unwrap();

        // Seed canonical staging SQL with explicit final SELECT list (no '*').
        let sql_rel = "models/staging/stg_test_raw_raw_customers.sql";
        let sql_key = project_fs::join_storage_key(&ctx, sql_rel);
        let sql = "with source as (\n  select * from {{ source('test_raw','raw_customers') }}\n)\nselect\n  customer_id_raw,\n  email_raw\nfrom source\n";
        ctx.storage()
            .put_bytes(&sql_key, sql.as_bytes(), "text/sql")
            .await
            .unwrap();

        let tool = ApplyNextCleanseSchemaBatchTool { datasets: None };
        let res = tool.call(serde_json::json!({}), &ctx).await.unwrap();
        assert!(
            res.get("ok").and_then(|v| v.as_bool()).unwrap_or(false),
            "unexpected tool response: {}",
            res
        );

        let got_plan = plan::load_cleanse_plan_by_key(&ctx, &plan_key)
            .await
            .unwrap()
            .unwrap();
        let t = got_plan
            .tasks
            .iter()
            .find(|t| t.dataset_id == "AwsDataCatalog.test_raw.raw_customers")
            .unwrap();
        let st = t
            .checklist
            .iter()
            .find(|it| it.checklist_item_id == plan::CHECKLIST_SCHEMA_CONTRACT)
            .map(|it| it.status)
            .unwrap();
        assert_eq!(st, plan::ChecklistItemStatus::Done);
    }

    #[tokio::test]
    async fn apply_next_model_schema_batch_patches_models_schema_yml() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let llm: Arc<dyn LargeLanguageModel> = Arc::new(ScriptedLlm {
            replies: Mutex::new(vec![serde_json::json!({
                "path": "models/schema.yml",
                "patch_text": "@@ ... @@\n+version: 2\n+\n+models:\n+  - name: dim_customers\n+    columns: []\n"
            }).to_string()]),
        });
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        let scope = RequestScope::parse("t", "w", "p").expect("valid test scope");
        let mut ctx = react_core::agent::AgentCtxBuilder::new(
            llm,
            storage.clone(),
            scope.clone(),
            keyspace,
            Arc::new(react_core::agent::DefaultPolicy),
        )
        .top_k(1)
        .per_step_timeout_secs(1)
        .max_steps(2)
        .thread_id("tid2".to_string())
        .agent_name("test".to_string())
        .resolved_config(Some(minimal_cfg()))
        .build();
        let providers =
            de_config::de_config_from_resolved(ctx.resolved_config().as_ref().unwrap()).unwrap();
        ctx.set_capability(Arc::new(ProvidersCfgCap(providers)));
        ctx.set_capability(Arc::new(WarehouseCap(
            Arc::new(NullWarehouseProvider) as Arc<dyn crate::providers::WarehouseProvider>
        )));

        // Seed a model plan with sql_model done and schema_contract pending.
        let plan_key = plan::new_model_plan_key(&ctx);
        let mut checklist = plan::canonical_task_checklist(TrackKind::Model);
        if let Some(item) = checklist
            .iter_mut()
            .find(|it| it.checklist_item_id == plan::CHECKLIST_SQL_MODEL)
        {
            item.status = plan::ChecklistItemStatus::Done;
        }
        let batches = vec![vec!["dim_customers".to_string()]];
        let p = plan::ModelPlan {
            plan_key: plan_key.clone(),
            status: plan::PlanStatus::Approved,
            project_snapshot: Default::default(),
            tasks: vec![plan::ModelTask {
                name: "dim_customers".to_string(),
                folder: plan::ModelFolder::Marts,
                goal: "g".to_string(),
                inputs: vec!["stg_test_raw_raw_customers".to_string()],
                expected_model_path: Some("models/marts/dim_customers.sql".to_string()),
                invariants: vec![],
                implementation_spec: Some(plan::ModelImplementationSpec {
                    spec_version: 1,
                    grain: "1 row per customer".to_string(),
                    inputs: vec!["stg_test_raw_raw_customers".to_string()],
                    joins: vec![],
                    metrics: vec![],
                    output_fields: vec![plan::OutputFieldSpec {
                        name: "customer_id".to_string(),
                        kind: plan::FieldKind::Clean,
                        lineage: vec![plan::FieldLineage::column(
                            plan::SourceFieldRef {
                                relation: None,
                                name: "customer_id".to_string(),
                            },
                            plan::LineageRole::Normalized,
                        )],
                        expression: "customer_id passthrough".to_string(),
                        data_type: None,
                        nullable: true,
                        description: None,
                    }],
                    assumptions: vec![],
                    evidence_claim_refs: observed_customer_key_claim(),
                }),
                source_schema: vec![],
                grounded_inputs: vec![plan::GroundedModelInput {
                    input_name: "stg_test_raw_raw_customers".to_string(),
                    model_rel_path: "models/staging/stg_test_raw_raw_customers.sql".to_string(),
                    relation_fqn: "db.schema.stg_test_raw_raw_customers".to_string(),
                    source_schema: vec![plan::SourceColumnDef {
                        name: "customer_id".to_string(),
                        data_type: "string".to_string(),
                    }],
                }],
                status: plan::TaskStatus::InProgress,
                checklist,
            }],
            batches: batches.clone(),
            work_groups: plan::canonical_work_groups_from_batches(&batches, "model"),
            mutations: vec![],
            progress: plan::PlanProgress::default(),
        };
        plan::save_model_plan(&ctx, &p).await.unwrap();
        ctx.storage()
            .put_bytes(
                &project_fs::join_storage_key(&ctx, "models/marts/dim_customers.sql"),
                b"select\n  customer_id\nfrom {{ ref('stg_test_raw_raw_customers') }}\n",
                "text/sql",
            )
            .await
            .unwrap();

        let tool = ApplyNextModelSchemaBatchTool { datasets: None };
        let res = tool.call(serde_json::json!({}), &ctx).await.unwrap();
        assert!(
            res.get("ok").and_then(|v| v.as_bool()).unwrap_or(false),
            "unexpected tool response: {}",
            res
        );

        let key = project_fs::join_storage_key(&ctx, project_fs::MODELS_SCHEMA_YML);
        let got = ctx.storage().get_bytes(&key).await.unwrap();
        let got = String::from_utf8_lossy(&got).to_string();
        assert!(got.contains("dim_customers"));
    }

    #[tokio::test]
    async fn apply_next_model_schema_batch_stops_when_local_failure_budget_exhausted() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let llm: Arc<dyn LargeLanguageModel> = Arc::new(ScriptedLlm {
            replies: Mutex::new(vec![]),
        });
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        let scope = RequestScope::parse("t", "w", "p").expect("valid test scope");
        let mut ctx = react_core::agent::AgentCtxBuilder::new(
            llm,
            storage.clone(),
            scope.clone(),
            keyspace,
            Arc::new(react_core::agent::DefaultPolicy),
        )
        .top_k(1)
        .per_step_timeout_secs(1)
        .max_steps(2)
        .thread_id("tid_model_lock".to_string())
        .agent_name("test".to_string())
        .resolved_config(Some(minimal_cfg()))
        .build();
        let providers =
            de_config::de_config_from_resolved(ctx.resolved_config().as_ref().unwrap()).unwrap();
        ctx.set_capability(Arc::new(ProvidersCfgCap(providers)));
        ctx.set_capability(Arc::new(WarehouseCap(
            Arc::new(NullWarehouseProvider) as Arc<dyn crate::providers::WarehouseProvider>
        )));

        let plan_key = plan::new_model_plan_key(&ctx);
        let mut checklist = plan::canonical_task_checklist(TrackKind::Model);
        if let Some(item) = checklist
            .iter_mut()
            .find(|it| it.checklist_item_id == plan::CHECKLIST_SQL_MODEL)
        {
            item.status = plan::ChecklistItemStatus::Done;
        }
        let batches = vec![vec!["dim_customers".to_string()]];
        let mut progress = plan::PlanProgress::default();
        progress.consecutive_batch_failures = controller_kernel::max_consecutive_batch_failures();
        let p = plan::ModelPlan {
            plan_key,
            status: plan::PlanStatus::Approved,
            project_snapshot: Default::default(),
            tasks: vec![plan::ModelTask {
                name: "dim_customers".to_string(),
                folder: plan::ModelFolder::Marts,
                goal: "g".to_string(),
                inputs: vec!["stg_test_raw_raw_customers".to_string()],
                expected_model_path: Some("models/marts/dim_customers.sql".to_string()),
                invariants: vec![],
                implementation_spec: Some(plan::ModelImplementationSpec {
                    spec_version: 1,
                    grain: "1 row per customer".to_string(),
                    inputs: vec!["stg_test_raw_raw_customers".to_string()],
                    joins: vec![],
                    metrics: vec![],
                    output_fields: vec![plan::OutputFieldSpec {
                        name: "customer_id".to_string(),
                        kind: plan::FieldKind::Clean,
                        lineage: vec![plan::FieldLineage::column(
                            plan::SourceFieldRef {
                                relation: None,
                                name: "customer_id".to_string(),
                            },
                            plan::LineageRole::Normalized,
                        )],
                        expression: "customer_id passthrough".to_string(),
                        data_type: None,
                        nullable: true,
                        description: None,
                    }],
                    assumptions: vec![],
                    evidence_claim_refs: observed_customer_key_claim(),
                }),
                source_schema: vec![],
                grounded_inputs: vec![plan::GroundedModelInput {
                    input_name: "stg_test_raw_raw_customers".to_string(),
                    model_rel_path: "models/staging/stg_test_raw_raw_customers.sql".to_string(),
                    relation_fqn: "db.schema.stg_test_raw_raw_customers".to_string(),
                    source_schema: vec![plan::SourceColumnDef {
                        name: "customer_id".to_string(),
                        data_type: "string".to_string(),
                    }],
                }],
                status: plan::TaskStatus::InProgress,
                checklist,
            }],
            batches: batches.clone(),
            work_groups: plan::canonical_work_groups_from_batches(&batches, "model"),
            mutations: vec![],
            progress,
        };
        plan::save_model_plan(&ctx, &p).await.unwrap();
        ctx.storage()
            .put_bytes(
                &project_fs::join_storage_key(&ctx, "models/marts/dim_customers.sql"),
                b"select\n  customer_id\nfrom {{ ref('stg_test_raw_raw_customers') }}\n",
                "text/sql",
            )
            .await
            .unwrap();

        let tool = ApplyNextModelSchemaBatchTool { datasets: None };
        let res = tool.call(serde_json::json!({}), &ctx).await.unwrap();
        assert_eq!(
            res.get("kind").and_then(|v| v.as_str()),
            Some("batch_locked")
        );
        assert_eq!(
            res.get("attempted_item_names")
                .and_then(|v| v.as_array())
                .map(|a| a.len()),
            Some(0)
        );
    }

    #[tokio::test]
    async fn apply_next_model_schema_batch_ignores_exec_ctx_checklist_override() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let llm: Arc<dyn LargeLanguageModel> = Arc::new(ScriptedLlm {
            replies: Mutex::new(vec![serde_json::json!({
                "path": "models/schema.yml",
                "patch_text": "@@ ... @@\n+version: 2\n+\n+models:\n+  - name: dim_customers\n+    columns: []\n"
            })
            .to_string()]),
        });
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        let scope = RequestScope::parse("t", "w", "p").expect("valid test scope");
        let mut ctx = react_core::agent::AgentCtxBuilder::new(
            llm,
            storage.clone(),
            scope.clone(),
            keyspace,
            Arc::new(react_core::agent::DefaultPolicy),
        )
        .top_k(1)
        .per_step_timeout_secs(1)
        .max_steps(2)
        .thread_id("tid_model_ctx".to_string())
        .agent_name("test".to_string())
        .resolved_config(Some(minimal_cfg()))
        .build();
        let providers =
            de_config::de_config_from_resolved(ctx.resolved_config().as_ref().unwrap()).unwrap();
        ctx.set_capability(Arc::new(ProvidersCfgCap(providers)));
        ctx.set_capability(Arc::new(WarehouseCap(
            Arc::new(NullWarehouseProvider) as Arc<dyn crate::providers::WarehouseProvider>
        )));

        let plan_key = plan::new_model_plan_key(&ctx);
        ctx.set_exec_ctx(Some({
            let mut ectx = ExecutionContext::default();
            ectx.set("plan_kind", serde_json::Value::String("model".to_string()));
            ectx.set("plan_key", serde_json::Value::String(plan_key.clone()));
            ectx.set("workgroup_id", serde_json::Value::String("wg".to_string()));
            ectx.set(
                "task_id",
                serde_json::Value::String("dim_customers".to_string()),
            );
            ectx.set(
                "checklist_item_id",
                serde_json::Value::String(plan::CHECKLIST_SQL_MODEL.to_string()),
            );
            ectx
        }));

        // Seed model SQL so allowed_columns can be derived (best-effort).
        let sql_rel = "models/marts/dim_customers.sql";
        let sql_key = project_fs::join_storage_key(&ctx, sql_rel);
        let sql = "with t as (\n  select 1 as customer_id, 'a@b.com' as email\n)\nselect\n  customer_id,\n  email\nfrom t\n";
        ctx.storage()
            .put_bytes(&sql_key, sql.as_bytes(), "text/sql")
            .await
            .unwrap();

        let mut checklist = plan::canonical_task_checklist(TrackKind::Model);
        if let Some(item) = checklist
            .iter_mut()
            .find(|it| it.checklist_item_id == plan::CHECKLIST_SQL_MODEL)
        {
            item.status = plan::ChecklistItemStatus::Done;
        }
        let batches = vec![vec!["dim_customers".to_string()]];
        let p = plan::ModelPlan {
            plan_key: plan_key.clone(),
            status: plan::PlanStatus::Approved,
            project_snapshot: Default::default(),
            tasks: vec![plan::ModelTask {
                name: "dim_customers".to_string(),
                folder: plan::ModelFolder::Marts,
                goal: "g".to_string(),
                inputs: vec!["stg_test_raw_raw_customers".to_string()],
                expected_model_path: Some(sql_rel.to_string()),
                invariants: vec![],
                implementation_spec: Some(plan::ModelImplementationSpec {
                    spec_version: 1,
                    grain: "1 row per customer".to_string(),
                    inputs: vec!["stg_test_raw_raw_customers".to_string()],
                    joins: vec![],
                    metrics: vec![],
                    output_fields: vec![plan::OutputFieldSpec {
                        name: "customer_id".to_string(),
                        kind: plan::FieldKind::Clean,
                        lineage: vec![plan::FieldLineage::column(
                            plan::SourceFieldRef {
                                relation: None,
                                name: "customer_id".to_string(),
                            },
                            plan::LineageRole::Normalized,
                        )],
                        expression: "customer_id passthrough".to_string(),
                        data_type: None,
                        nullable: true,
                        description: None,
                    }],
                    assumptions: vec![],
                    evidence_claim_refs: observed_customer_key_claim(),
                }),
                source_schema: vec![],
                grounded_inputs: vec![plan::GroundedModelInput {
                    input_name: "stg_test_raw_raw_customers".to_string(),
                    model_rel_path: "models/staging/stg_test_raw_raw_customers.sql".to_string(),
                    relation_fqn: "db.schema.stg_test_raw_raw_customers".to_string(),
                    source_schema: vec![plan::SourceColumnDef {
                        name: "customer_id".to_string(),
                        data_type: "string".to_string(),
                    }],
                }],
                status: plan::TaskStatus::InProgress,
                checklist,
            }],
            batches: batches.clone(),
            work_groups: plan::canonical_work_groups_from_batches(&batches, "model"),
            mutations: vec![],
            progress: plan::PlanProgress::default(),
        };
        plan::save_model_plan(&ctx, &p).await.unwrap();
        ctx.storage()
            .put_bytes(
                &project_fs::join_storage_key(&ctx, "models/marts/dim_customers.sql"),
                b"select\n  customer_id\nfrom {{ ref('stg_test_raw_raw_customers') }}\n",
                "text/sql",
            )
            .await
            .unwrap();

        let tool = ApplyNextModelSchemaBatchTool { datasets: None };
        let res = tool.call(serde_json::json!({}), &ctx).await.unwrap();
        assert!(
            res.get("ok").and_then(|v| v.as_bool()).unwrap_or(false),
            "unexpected tool response: {}",
            res
        );

        let got_plan = plan::load_model_plan_by_key(&ctx, &plan_key)
            .await
            .unwrap()
            .unwrap();
        let t = got_plan
            .tasks
            .iter()
            .find(|t| t.name == "dim_customers")
            .unwrap();
        let st = t
            .checklist
            .iter()
            .find(|it| it.checklist_item_id == plan::CHECKLIST_SCHEMA_CONTRACT)
            .map(|it| it.status)
            .unwrap();
        assert_eq!(st, plan::ChecklistItemStatus::Done);
    }

    #[tokio::test]
    async fn apply_next_model_schema_batch_includes_allowed_columns_in_payload() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let reply = serde_json::json!({
            "path": "models/schema.yml",
            "patch_text": "@@ ... @@\n+version: 2\n+\n+models:\n+  - name: dim_customers\n+    columns: []\n"
        })
        .to_string();
        let llm = Arc::new(InspectingLlm {
            reply,
            saw_allowed_columns: Mutex::new(false),
        });
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        let scope = RequestScope::parse("t", "w", "p").expect("valid test scope");
        let mut ctx = react_core::agent::AgentCtxBuilder::new(
            llm.clone(),
            storage.clone(),
            scope.clone(),
            keyspace,
            Arc::new(react_core::agent::DefaultPolicy),
        )
        .top_k(1)
        .per_step_timeout_secs(1)
        .max_steps(2)
        .thread_id("tid3".to_string())
        .agent_name("test".to_string())
        .resolved_config(Some(minimal_cfg()))
        .build();
        let providers =
            de_config::de_config_from_resolved(ctx.resolved_config().as_ref().unwrap()).unwrap();
        ctx.set_capability(Arc::new(ProvidersCfgCap(providers)));
        ctx.set_capability(Arc::new(WarehouseCap(
            Arc::new(NullWarehouseProvider) as Arc<dyn crate::providers::WarehouseProvider>
        )));

        // Seed model SQL so allowed_columns can be derived.
        let sql_rel = "models/marts/dim_customers.sql";
        let sql_key = project_fs::join_storage_key(&ctx, sql_rel);
        let sql = "with t as (\n  select 1 as customer_id, 'a@b.com' as email\n)\nselect\n  customer_id,\n  email\nfrom t\n";
        ctx.storage()
            .put_bytes(&sql_key, sql.as_bytes(), "text/sql")
            .await
            .unwrap();

        let plan_key = plan::new_model_plan_key(&ctx);
        let mut checklist = plan::canonical_task_checklist(TrackKind::Model);
        if let Some(item) = checklist
            .iter_mut()
            .find(|it| it.checklist_item_id == plan::CHECKLIST_SQL_MODEL)
        {
            item.status = plan::ChecklistItemStatus::Done;
        }
        let batches = vec![vec!["dim_customers".to_string()]];
        let p = plan::ModelPlan {
            plan_key: plan_key.clone(),
            status: plan::PlanStatus::Approved,
            project_snapshot: Default::default(),
            tasks: vec![plan::ModelTask {
                name: "dim_customers".to_string(),
                folder: plan::ModelFolder::Marts,
                goal: "g".to_string(),
                inputs: vec!["stg_test_raw_raw_customers".to_string()],
                expected_model_path: Some(sql_rel.to_string()),
                invariants: vec![],
                implementation_spec: Some(plan::ModelImplementationSpec {
                    spec_version: 1,
                    grain: "1 row per customer".to_string(),
                    inputs: vec!["stg_test_raw_raw_customers".to_string()],
                    joins: vec![],
                    metrics: vec![],
                    output_fields: vec![plan::OutputFieldSpec {
                        name: "customer_id".to_string(),
                        kind: plan::FieldKind::Clean,
                        lineage: vec![plan::FieldLineage::column(
                            plan::SourceFieldRef {
                                relation: None,
                                name: "customer_id".to_string(),
                            },
                            plan::LineageRole::Normalized,
                        )],
                        expression: "customer_id passthrough".to_string(),
                        data_type: None,
                        nullable: true,
                        description: None,
                    }],
                    assumptions: vec![],
                    evidence_claim_refs: observed_customer_key_claim(),
                }),
                source_schema: vec![],
                grounded_inputs: vec![plan::GroundedModelInput {
                    input_name: "stg_test_raw_raw_customers".to_string(),
                    model_rel_path: "models/staging/stg_test_raw_raw_customers.sql".to_string(),
                    relation_fqn: "db.schema.stg_test_raw_raw_customers".to_string(),
                    source_schema: vec![plan::SourceColumnDef {
                        name: "customer_id".to_string(),
                        data_type: "string".to_string(),
                    }],
                }],
                status: plan::TaskStatus::InProgress,
                checklist,
            }],
            batches: batches.clone(),
            work_groups: plan::canonical_work_groups_from_batches(&batches, "model"),
            mutations: vec![],
            progress: plan::PlanProgress::default(),
        };
        plan::save_model_plan(&ctx, &p).await.unwrap();
        ctx.storage()
            .put_bytes(
                &project_fs::join_storage_key(&ctx, "models/marts/dim_customers.sql"),
                b"select\n  customer_id\nfrom {{ ref('stg_test_raw_raw_customers') }}\n",
                "text/sql",
            )
            .await
            .unwrap();

        let tool = ApplyNextModelSchemaBatchTool { datasets: None };
        let res = tool.call(serde_json::json!({}), &ctx).await.unwrap();
        assert!(
            res.get("ok").and_then(|v| v.as_bool()).unwrap_or(false),
            "unexpected tool response: {}",
            res
        );

        let got = ctx
            .storage()
            .get_bytes(&project_fs::join_storage_key(
                &ctx,
                project_fs::MODELS_SCHEMA_YML,
            ))
            .await
            .unwrap();
        let got = String::from_utf8_lossy(&got).to_string();
        assert!(
            got.contains("customer_id"),
            "expected generated schema YAML to include SQL output column, got: {got}"
        );
    }

    #[tokio::test]
    async fn apply_next_model_schema_batch_strips_stg_models_from_models_schema_yml() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let llm: Arc<dyn LargeLanguageModel> = Arc::new(ScriptedLlm {
            replies: Mutex::new(vec![serde_json::json!({
                "path": "models/schema.yml",
                "patch_text": "@@ ... @@\n+version: 2\n+\n+models:\n+  - name: stg_test_raw_raw_customers\n+    columns: []\n+  - name: dim_customers\n+    columns: []\n"
            }).to_string()]),
        });
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        let scope = RequestScope::parse("t", "w", "p").expect("valid test scope");
        let mut ctx = react_core::agent::AgentCtxBuilder::new(
            llm,
            storage.clone(),
            scope.clone(),
            keyspace,
            Arc::new(react_core::agent::DefaultPolicy),
        )
        .top_k(1)
        .per_step_timeout_secs(1)
        .max_steps(2)
        .thread_id("tid4".to_string())
        .agent_name("test".to_string())
        .resolved_config(Some(minimal_cfg()))
        .build();
        let providers =
            de_config::de_config_from_resolved(ctx.resolved_config().as_ref().unwrap()).unwrap();
        ctx.set_capability(Arc::new(ProvidersCfgCap(providers)));
        ctx.set_capability(Arc::new(WarehouseCap(
            Arc::new(NullWarehouseProvider) as Arc<dyn crate::providers::WarehouseProvider>
        )));

        // Seed model plan.
        let plan_key = plan::new_model_plan_key(&ctx);
        let mut checklist = plan::canonical_task_checklist(TrackKind::Model);
        if let Some(item) = checklist
            .iter_mut()
            .find(|it| it.checklist_item_id == plan::CHECKLIST_SQL_MODEL)
        {
            item.status = plan::ChecklistItemStatus::Done;
        }
        let batches = vec![vec!["dim_customers".to_string()]];
        let p = plan::ModelPlan {
            plan_key: plan_key.clone(),
            status: plan::PlanStatus::Approved,
            project_snapshot: Default::default(),
            tasks: vec![plan::ModelTask {
                name: "dim_customers".to_string(),
                folder: plan::ModelFolder::Marts,
                goal: "g".to_string(),
                inputs: vec!["stg_test_raw_raw_customers".to_string()],
                expected_model_path: Some("models/marts/dim_customers.sql".to_string()),
                invariants: vec![],
                implementation_spec: Some(plan::ModelImplementationSpec {
                    spec_version: 1,
                    grain: "1 row per customer".to_string(),
                    inputs: vec!["stg_test_raw_raw_customers".to_string()],
                    joins: vec![],
                    metrics: vec![],
                    output_fields: vec![plan::OutputFieldSpec {
                        name: "customer_id".to_string(),
                        kind: plan::FieldKind::Clean,
                        lineage: vec![plan::FieldLineage::column(
                            plan::SourceFieldRef {
                                relation: None,
                                name: "customer_id".to_string(),
                            },
                            plan::LineageRole::Normalized,
                        )],
                        expression: "customer_id passthrough".to_string(),
                        data_type: None,
                        nullable: true,
                        description: None,
                    }],
                    assumptions: vec![],
                    evidence_claim_refs: observed_customer_key_claim(),
                }),
                source_schema: vec![],
                grounded_inputs: vec![plan::GroundedModelInput {
                    input_name: "stg_test_raw_raw_customers".to_string(),
                    model_rel_path: "models/staging/stg_test_raw_raw_customers.sql".to_string(),
                    relation_fqn: "db.schema.stg_test_raw_raw_customers".to_string(),
                    source_schema: vec![plan::SourceColumnDef {
                        name: "customer_id".to_string(),
                        data_type: "string".to_string(),
                    }],
                }],
                status: plan::TaskStatus::InProgress,
                checklist,
            }],
            batches: batches.clone(),
            work_groups: plan::canonical_work_groups_from_batches(&batches, "model"),
            mutations: vec![],
            progress: plan::PlanProgress::default(),
        };
        plan::save_model_plan(&ctx, &p).await.unwrap();
        ctx.storage()
            .put_bytes(
                &project_fs::join_storage_key(&ctx, "models/marts/dim_customers.sql"),
                b"select\n  customer_id\nfrom {{ ref('stg_test_raw_raw_customers') }}\n",
                "text/sql",
            )
            .await
            .unwrap();

        let tool = ApplyNextModelSchemaBatchTool { datasets: None };
        let res = tool.call(serde_json::json!({}), &ctx).await.unwrap();
        assert!(
            res.get("ok").and_then(|v| v.as_bool()).unwrap_or(false),
            "unexpected tool response: {}",
            res
        );

        let key = project_fs::join_storage_key(&ctx, project_fs::MODELS_SCHEMA_YML);
        let got = ctx.storage().get_bytes(&key).await.unwrap();
        let got = String::from_utf8_lossy(&got).to_string();
        assert!(!got.contains("stg_test_raw_raw_customers"));
        assert!(got.contains("dim_customers"));
    }
}
