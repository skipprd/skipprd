use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

use crate::providers::DatasetCatalogProvider;
use react_core::agent::AgentCtx;
use react_core::llm::LlmCallOptions;
use react_core::tools::Tool;

use crate::chunk_progress_contract;
use crate::controller_kernel;
use crate::naming;
use crate::plan;
use crate::project_fs;
use crate::references::DatasetRef;
use crate::schema_policy;
use crate::tools::files_tool;

fn escape_yaml_doc_preamble(s: String) -> String {
    // serde_yaml may emit a leading `---\n`; keep stored files clean and consistent.
    s.trim_start_matches("---\n").to_string()
}

fn strip_where_keys(v: &mut serde_yaml::Value) {
    // Deterministic sanitization: remove `where:` keys anywhere in the YAML subtree.
    // This avoids schema-yml validation failures due to referencing *_raw columns that are not present
    // in the sibling staging SQL output. (Tests are optional; correctness > strictness here.)
    match v {
        serde_yaml::Value::Mapping(m) => {
            m.remove(&serde_yaml::Value::String("where".to_string()));
            for (_k, vv) in m.iter_mut() {
                strip_where_keys(vv);
            }
        }
        serde_yaml::Value::Sequence(seq) => {
            for vv in seq.iter_mut() {
                strip_where_keys(vv);
            }
        }
        _ => {}
    }
}

fn sanitize_staging_schema_yml_to_allowed_columns(
    yml_text: &str,
    model_name: &str,
    allowed_columns: &[String],
) -> Result<String, String> {
    let allowed: std::collections::HashSet<String> = allowed_columns
        .iter()
        .cloned()
        .collect::<std::collections::HashSet<_>>();
    let mut root: serde_yaml::Value =
        serde_yaml::from_str(yml_text).map_err(|e| format!("invalid YAML: {}", e))?;

    // Remove problematic `where:` keys (tests) first.
    strip_where_keys(&mut root);

    // Filter models[].columns[].name to allowed set for the specific model.
    let Some(root_map) = root.as_mapping_mut() else {
        return Ok(yml_text.to_string());
    };
    let Some(models) = root_map
        .get_mut(&serde_yaml::Value::String("models".to_string()))
        .and_then(|v| v.as_sequence_mut())
    else {
        return Ok(yml_text.to_string());
    };

    for m in models.iter_mut() {
        let Some(mm) = m.as_mapping_mut() else {
            continue;
        };
        let name = mm
            .get(&serde_yaml::Value::String("name".to_string()))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if name != model_name {
            continue;
        }
        let Some(cols) = mm
            .get_mut(&serde_yaml::Value::String("columns".to_string()))
            .and_then(|v| v.as_sequence_mut())
        else {
            continue;
        };
        cols.retain(|c| {
            let Some(cm) = c.as_mapping() else {
                return true;
            };
            let col = cm
                .get(&serde_yaml::Value::String("name".to_string()))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if col.is_empty() {
                return false;
            }
            allowed.contains(col)
        });
    }

    serde_yaml::to_string(&root)
        .map(escape_yaml_doc_preamble)
        .map_err(|e| format!("failed to re-serialize YAML: {}", e))
}

fn rewrite_final_select_wildcard(sql_text: &str, allowed_columns: &[String]) -> Option<String> {
    if allowed_columns.is_empty() {
        return None;
    }
    let lowered = sql_text.to_ascii_lowercase();
    let select_idx = lowered.rfind("select")?;
    let from_search_start = select_idx + "select".len();
    let from_rel = lowered[from_search_start..].find("from")?;
    let from_idx = from_search_start + from_rel;
    let select_expr = sql_text[from_search_start..from_idx].trim();
    let wildcard = select_expr == "*" || select_expr.ends_with(".*");
    if !wildcard {
        return None;
    }

    let projected = allowed_columns
        .iter()
        .map(|c| c.trim())
        .filter(|c| !c.is_empty())
        .map(|c| format!("  {c}"))
        .collect::<Vec<_>>();
    if projected.is_empty() {
        return None;
    }

    let mut out = String::with_capacity(sql_text.len() + projected.len() * 8);
    out.push_str(&sql_text[..select_idx]);
    out.push_str("select\n");
    out.push_str(&projected.join(",\n"));
    out.push('\n');
    out.push_str(&sql_text[from_idx..]);
    Some(out)
}

use super::model_authoring_engine::extract_string_arg;

fn schema_yml_sys_prompt_staging() -> String {
    vec![
        "You are an expert analytics engineer.".to_string(),
        "Task: author a dbt *silver schema* YAML for ONE silver model file under models/staging/.".to_string(),
        "Requirements:".to_string(),
        "- Return a single-file patch as `patch_text` using Cursor/Aider hunks-only format (MUST).".to_string(),
        "- Patch MUST modify ONLY expected_rel_path (no other files).".to_string(),
        "- Do NOT add or reference columns not present in allowed_columns.".to_string(),
        "- Do NOT re-declare sources; source definitions belong in models/schema.yml or other authorized files.".to_string(),
        "- IMPORTANT: contract enforcement is disabled. Do NOT set models[].config.contract.enforced=true.".to_string(),
        "- Prefer including all allowed_columns under models[].columns, but it is OK if some are missing while iterating.".to_string(),
        "- data_type is optional (preferred when known, omit rather than guessing).".to_string(),
        "- Column names must match allowed_columns exactly (no comments or helper markers as column names).".to_string(),
        "".to_string(),
        crate::prompts::patch_contract::llm_patch_response_contract(),
    ]
    .join("\n")
}

fn schema_yml_sys_prompt_models_schema_yml() -> String {
    vec![
        "You are an expert analytics engineer.".to_string(),
        "Task: update models/schema.yml to add or update dbt model documentation/tests for a small set of gold models.".to_string(),
        "Requirements:".to_string(),
        "- Return a single-file patch as `patch_text` using Cursor/Aider hunks-only format (MUST).".to_string(),
        "- Patch MUST modify ONLY expected_rel_path (no other files).".to_string(),
        "- Do NOT create additional YAML files; use models/schema.yml only.".to_string(),
        "- IMPORTANT (ownership): do NOT add staging (stg_*) models to models/schema.yml. Staging docs/tests must be in models/staging/*.yml.".to_string(),
        "- IMPORTANT (grounding): For each model, you will be given allowed_columns derived from its SQL. Do NOT create tests or where: predicates that reference columns not in allowed_columns.".to_string(),
        "- If allowed_columns is empty/unavailable for a model, you MAY update docs/descriptions, but you MUST NOT add tests for that model.".to_string(),
        "- Keep output concise: only touch the specified model names; preserve existing content unrelated to those models.".to_string(),
        "".to_string(),
        "Business-grade documentation (CRITICAL):".to_string(),
        "- For each touched model, the model description MUST include:".to_string(),
        "  - Grain (one sentence).".to_string(),
        "  - Business question / decision it supports (one sentence).".to_string(),
        "  - Time axis semantics if the model is time-based (what the date/timestamp means).".to_string(),
        "- For key metric columns, include a concrete definition + caveats (in plain English).".to_string(),
        "- Prefer a few high-signal tests (unique/not_null/relationships) only when grounded by allowed_columns; do not add speculative tests.".to_string(),
        "".to_string(),
        crate::prompts::patch_contract::llm_patch_response_contract(),
    ]
    .join("\n")
}

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

        // Plan auto-heal (semantic): validate + single repair attempt before executing.
        let v = plan::ensure_cleanse_plan_semantically_valid_or_repaired(&mut plan);
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

        let instructions = extract_string_arg(&args, "instructions")
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
        let mut auto_healed_wildcard_sql_dataset_ids: Vec<String> = Vec::new();
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
                    let from_plan = plan
                        .tasks
                        .iter()
                        .find(|t| t.dataset_id == *ds)
                        .and_then(|t| {
                            t.implementation_spec.as_ref().map(|spec| {
                                spec.output_fields
                                    .iter()
                                    .map(|f| f.name.trim().to_string())
                                    .filter(|n| !n.is_empty())
                                    .collect::<Vec<String>>()
                            })
                        })
                        .unwrap_or_default();
                    if !from_plan.is_empty() {
                        tracing::warn!(
                            "{ds}: SQL parser could not extract columns from {sql_rel} ({e}); \
                             using plan output_fields as source of truth ({} cols)",
                            from_plan.len()
                        );
                        // Best-effort auto-heal: rewrite SELECT * to explicit columns if possible.
                        if let Some(rewritten_sql) =
                            rewrite_final_select_wildcard(&sql_text, &from_plan)
                        {
                            if rewritten_sql != sql_text {
                                if let Err(write_err) = project_fs::write_project_file_via_patch(
                                    ctx,
                                    self.datasets.as_ref(),
                                    &sql_rel,
                                    &rewritten_sql,
                                    "text/sql",
                                )
                                .await
                                {
                                    tracing::warn!(
                                        "{ds}: auto-heal wildcard rewrite failed: {write_err}"
                                    );
                                } else {
                                    auto_healed_wildcard_sql_dataset_ids.push(ds.clone());
                                }
                            }
                        }
                        from_plan
                    } else {
                        failed.push(ds.clone());
                        let msg = format!(
                            "{ds}: cannot determine output columns — SQL parser failed ({e}) \
                             and plan output_fields are empty or generic. \
                             The plan must provide concrete output_fields for this dataset."
                        );
                        errors.push(msg.clone());
                        plan_violations.push(crate::tools::batch_contracts::PlanViolationBrief {
                            task_id: ds.clone(),
                            evidence: msg,
                        });
                        continue;
                    }
                }
            };
            let mut allowed_cols = allowed_cols;
            allowed_cols.sort();
            allowed_cols.dedup();

            let user_payload = serde_json::json!({
                "dataset_id": ds,
                "model_name": model_name,
                "expected_model_sql_path": sql_rel,
                "allowed_columns": allowed_cols,
                "instructions": instructions,
            })
            .to_string();

            let (outcome, _notes) = match crate::patch_protocol::llm_patch_loop_single_file(
                ctx,
                self.datasets.as_ref(),
                schema_yml_sys_prompt_staging(),
                user_payload,
                &yml_rel,
                6,
                Some(LlmCallOptions {
                    prompt_id: "data_engineer.apply_next_schema_batch.staging_schema_patch",
                    thread_id: ctx.thread_id().clone(),
                    max_output_tokens: Some(
                        crate::patch_protocol::default_patch_loop_max_output_tokens(),
                    ),
                    reasoning_effort: Some(react_core::llm::ReasoningEffort::Low),
                    ..Default::default()
                }),
            )
            .await
            {
                Ok(v) => v,
                Err(e) => {
                    failed.push(ds.clone());
                    errors.push(format!("{ds}: schema patch failed: {e}"));
                    continue;
                }
            };

            // Deterministic safety net: even with `allowed_columns` grounding, models sometimes invent
            // column names (e.g. *_norm) that are not actually produced by the sibling SQL. Rather than
            // failing the whole batch repeatedly, sanitize the YAML to only allowed columns and retry validation.
            let mut outcome = outcome;
            if outcome.rel_path.starts_with("models/staging/") && outcome.rel_path.ends_with(".yml")
            {
                match sanitize_staging_schema_yml_to_allowed_columns(
                    &outcome.content,
                    &model_name,
                    &allowed_cols,
                ) {
                    Ok(s) => outcome.content = s,
                    Err(_) => {
                        // If sanitization fails (e.g. invalid YAML), validation will surface the error.
                    }
                }
            }

            // Validate schema contract against sibling SQL output columns.
            if let Err(e) =
                files_tool::validate_staging_schema_ymls(ctx, std::slice::from_ref(&outcome)).await
            {
                failed.push(ds.clone());
                errors.push(format!("{ds}: invalid staging schema yml: {e}"));
                continue;
            }

            if let Err(e) = project_fs::write_project_file_via_patch(
                ctx,
                self.datasets.as_ref(),
                &yml_rel,
                &outcome.content,
                "text/yaml",
            )
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

        // Plan auto-heal (semantic): validate + single repair attempt before executing.
        let stg = crate::dataset_truth::discover_staging_models_from_storage(ctx).await;
        let v =
            plan::ensure_model_plan_semantically_valid_or_repaired(&mut plan, &stg.allowed_models);
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

        let instructions = extract_string_arg(&args, "instructions")
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

        // Provide just the model names + expected SQL rel paths to keep the patch focused.
        let mut models: Vec<Value> = Vec::new();
        let mut touched: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut allowed_by_model: std::collections::HashMap<
            String,
            schema_policy::ModelAllowedColumns,
        > = std::collections::HashMap::new();
        for n in names.iter() {
            if let Some(t) = plan.tasks.iter().find(|t| t.name == *n) {
                touched.insert(t.name.clone());
                // Derive allowed columns from the model SQL output projection (best-effort).
                let mut allowed = schema_policy::ModelAllowedColumns::default();
                if let Some(ref rel) = t.expected_model_path {
                    match project_fs::read_project_file_text(ctx, rel).await {
                        Ok(Some(sql_text)) => {
                            match files_tool::extract_final_select_output_columns(&sql_text) {
                                Ok(cols) => {
                                    allowed.allowed_columns =
                                        cols.into_iter().collect::<std::collections::HashSet<_>>();
                                }
                                Err(e) => {
                                    allowed.error = Some(e);
                                }
                            }
                        }
                        Ok(None) => {
                            allowed.error = Some(format!("missing model SQL at {rel}"));
                        }
                        Err(e) => {
                            allowed.error = Some(format!("missing model SQL at {rel}: {e}"));
                        }
                    }
                } else {
                    allowed.error = Some("expected_model_path missing in plan task".to_string());
                }
                allowed_by_model.insert(t.name.clone(), allowed.clone());
                models.push(serde_json::json!({
                    "name": t.name,
                    "folder": t.folder,
                    "expected_model_path": t.expected_model_path,
                    "goal": t.goal,
                    "inputs": t.inputs,
                    "invariants": t.invariants,
                    "allowed_columns": allowed.allowed_columns.iter().cloned().collect::<Vec<_>>(),
                    "allowed_columns_error": allowed.error,
                }));
            } else {
                models.push(serde_json::json!({ "name": n }));
            }
        }
        let user_payload = serde_json::json!({
            "models": models,
            "instructions": instructions,
            "instruction": "Add or update entries under top-level 'models:' for these names only. Keep other models untouched.",
        })
        .to_string();

        let (outcome, _notes) = match crate::patch_protocol::llm_patch_loop_single_file(
            ctx,
            self.datasets.as_ref(),
            schema_yml_sys_prompt_models_schema_yml(),
            user_payload,
            expected_rel,
            6,
            Some(LlmCallOptions {
                prompt_id: "data_engineer.apply_next_schema_batch.models_schema_patch",
                thread_id: ctx.thread_id().clone(),
                max_output_tokens: Some(
                    crate::patch_protocol::default_patch_loop_max_output_tokens(),
                ),
                reasoning_effort: Some(react_core::llm::ReasoningEffort::Low),
                ..Default::default()
            }),
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                return crate::tools::batch_schema_runner::fail_model_schema_batch(
                    ctx,
                    &mut plan,
                    &attempted_names,
                    &checklist_item_id,
                    format!("models/schema.yml patch failed: {e}"),
                )
                .await;
            }
        };

        // Deterministic post-check: enforce schema ownership + strip unsafe tests for touched models.
        let (sanitized_text, warnings) = match schema_policy::sanitize_models_schema_yml(
            &outcome.content,
            &touched,
            &allowed_by_model,
        ) {
            Ok(v) => v,
            Err(e) => {
                return crate::tools::batch_schema_runner::fail_model_schema_batch(
                    ctx,
                    &mut plan,
                    &attempted_names,
                    &checklist_item_id,
                    format!("models/schema.yml post-check failed: {e}"),
                )
                .await;
            }
        };

        if let Err(e) = project_fs::write_project_file_via_patch(
            ctx,
            self.datasets.as_ref(),
            expected_rel,
            &sanitized_text,
            "text/yaml",
        )
        .await
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
                                plan::lineage_role::PASSTHROUGH,
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
                                plan::lineage_role::PASSTHROUGH,
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

        // Seed staging SQL with wildcard final projection so the tool exercises deterministic
        // wildcard auto-heal before writing schema YAML.
        let sql_rel = "models/staging/stg_test_raw_raw_customers.sql";
        let sql_key = project_fs::join_storage_key(&ctx, sql_rel);
        let sql = "with source as (\n  select * from {{ source('test_raw','raw_customers') }}\n)\nselect * from source\n";
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

        let healed_sql = ctx.storage().get_bytes(&sql_key).await.unwrap();
        let healed_sql = String::from_utf8_lossy(&healed_sql).to_string();
        assert!(
            healed_sql.contains("customer_id_raw") && healed_sql.contains("email_raw"),
            "expected wildcard auto-heal to expand final SELECT columns, got: {}",
            healed_sql
        );
        let healed_ds = res
            .get("auto_healed_wildcard_sql_dataset_ids")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        assert!(healed_ds
            .iter()
            .any(|v| v.as_str() == Some("AwsDataCatalog.test_raw.raw_customers")));
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
                            plan::lineage_role::PASSTHROUGH,
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
                            plan::lineage_role::PASSTHROUGH,
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
                            plan::lineage_role::NORMALIZED,
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
                            plan::lineage_role::NORMALIZED,
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
                            plan::lineage_role::NORMALIZED,
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
                            plan::lineage_role::NORMALIZED,
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

        let tool = ApplyNextModelSchemaBatchTool { datasets: None };
        let res = tool.call(serde_json::json!({}), &ctx).await.unwrap();
        assert!(
            res.get("ok").and_then(|v| v.as_bool()).unwrap_or(false),
            "unexpected tool response: {}",
            res
        );

        let saw = *llm
            .saw_allowed_columns
            .lock()
            .map_err(|_| "mutex poisoned".to_string())
            .unwrap();
        assert!(saw, "expected LLM payload to include allowed_columns");
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
                            plan::lineage_role::NORMALIZED,
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
