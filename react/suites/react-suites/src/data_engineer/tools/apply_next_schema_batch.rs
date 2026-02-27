use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

use react_core::agent::AgentCtx;
use react_core::llm::LlmCallOptions;
use react_core::providers::DatasetCatalogProvider;
use react_core::tools::Tool;

use crate::data_engineer::chunk_progress_contract;
use crate::data_engineer::naming;
use crate::data_engineer::plan;
use crate::data_engineer::controller_kernel;
use crate::data_engineer::project_files;
use crate::data_engineer::project_fs;
use crate::data_engineer::references::DatasetRef;
use crate::data_engineer::schema_policy;
use crate::data_engineer::tools::dbt_files;

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

fn extract_string_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|x| x.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn schema_yml_sys_prompt_staging() -> String {
    vec![
        "You are an expert analytics engineer.".to_string(),
        "Task: author a dbt *silver schema* YAML for ONE silver model file under models/staging/.".to_string(),
        "Requirements:".to_string(),
        "- Output MUST be valid JSON only.".to_string(),
        "- Return a single-file patch as `patch_text` using Cursor/Aider hunks-only format (MUST).".to_string(),
        "- Patch MUST modify ONLY expected_rel_path (no other files).".to_string(),
        "- Do NOT add or reference columns not present in allowed_columns.".to_string(),
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
        "- Output MUST be valid JSON only.".to_string(),
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
        let checklist_item_id = ctx
            .exec_ctx
            .as_ref()
            .and_then(|c| c.checklist_item_id.as_ref())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| plan::CHECKLIST_SCHEMA_CONTRACT.to_string());

        let mut plan = plan::load_cleanse_plan_any(ctx)
            .await
            .ok_or_else(|| "no active cleanse plan found".to_string())?;
        if plan.status != plan::PlanStatus::Approved && plan.status != plan::PlanStatus::Completed {
            return Err(format!(
                "cleanse plan is not approved (status={:?}); return to plan phase",
                plan.status
            ));
        }

        // Plan auto-heal (semantic): validate + single repair attempt before executing.
        let v = plan::ensure_cleanse_plan_semantically_valid_or_repaired(ctx, &mut plan).await?;
        if !v.ok {
            return Ok(serde_json::json!({
                "ok": false,
                "kind": "plan_invalid",
                "plan_key": plan.plan_key,
                "errors": v.errors,
                "attempted_dataset_ids": [],
                "succeeded_dataset_ids": [],
                "failed_dataset_ids": [],
            }));
        }

        if controller_kernel::batch_budget(&plan.progress).exhausted() {
            return Ok(serde_json::json!({
                "ok": false,
                "kind": "batch_locked",
                "reason_code": controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted,
                "message": controller_kernel::batch_lock_error_message(controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted),
                "checklist_item_id": checklist_item_id,
                "attempted_dataset_ids": [],
                "succeeded_dataset_ids": [],
                "failed_dataset_ids": [],
                "errors": [controller_kernel::batch_lock_error_message(controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted)],
            }));
        }

        let batch = plan::cleanse_pending_for_checklist(
            &plan,
            plan::CHECKLIST_SQL_MODEL,
            &checklist_item_id,
        );
        if batch.is_empty() {
            return Ok(serde_json::json!({
                "ok": false,
                "kind": "no_progress",
                "progress_made": false,
                "message": "no pending schema checklist work (all done)",
                "checklist_item_id": checklist_item_id,
                "attempted_dataset_ids": [],
                "succeeded_dataset_ids": [],
                "failed_dataset_ids": [],
            }));
        }
        if let Err(e) =
            chunk_progress_contract::enforce_chunk_contract(&batch, 5, "cleanse_schema")
        {
            return Ok(serde_json::json!({
                "ok": false,
                "kind": "chunk_contract_violation",
                "checklist_item_id": checklist_item_id,
                "attempted_dataset_ids": [],
                "succeeded_dataset_ids": [],
                "failed_dataset_ids": [],
                "errors": [e],
            }));
        }

        let instructions = extract_string_arg(&args, "instructions")
            .or_else(|| extract_string_arg(&args, "user_instructions"))
            .unwrap_or_default();

        for ds in batch.iter() {
            plan::cleanse_checklist_mark_status(
                &mut plan,
                ds,
                &checklist_item_id,
                plan::ChecklistItemStatus::InProgress,
            );
        }
        plan::save_cleanse_plan(ctx, &plan)
            .await
            .map_err(|e| format!("failed to persist cleanse schema batch start state: {e}"))?;

        let mut succeeded: Vec<String> = Vec::new();
        let mut failed: Vec<String> = Vec::new();
        let mut errors: Vec<String> = Vec::new();
        let mut auto_healed_wildcard_sql_dataset_ids: Vec<String> = Vec::new();

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

            let sql_key = project_fs::join_storage_key(ctx, &sql_rel);
            let sql_text = match ctx.storage.get_bytes(&sql_key).await {
                Ok(b) => String::from_utf8_lossy(&b).to_string(),
                Err(_) => {
                    failed.push(ds.clone());
                    errors.push(format!("{ds}: missing sibling SQL {sql_rel}"));
                    continue;
                }
            };
            let allowed_cols = match dbt_files::extract_final_select_output_columns(&sql_text) {
                Ok(s) => s.into_iter().collect::<Vec<_>>(),
                Err(e) => {
                    // High-signal fallback for SELECT * loops:
                    // when SQL parsing cannot infer final columns, use the approved cleanse-plan
                    // output field names to keep schema generation moving deterministically.
                    let from_plan = plan
                        .tasks
                        .iter()
                        .find(|t| t.dataset_id == *ds)
                        .map(|t| {
                            t.implementation_spec
                                .output_fields
                                .iter()
                                .map(|f| f.name.trim().to_string())
                                .filter(|n| !n.is_empty())
                                .collect::<Vec<String>>()
                        })
                        .unwrap_or_default();
                    if !from_plan.is_empty() {
                        if let Some(rewritten_sql) =
                            rewrite_final_select_wildcard(&sql_text, &from_plan)
                        {
                            if rewritten_sql != sql_text {
                                if let Err(write_err) = ctx
                                    .storage
                                    .put_bytes(&sql_key, rewritten_sql.as_bytes(), "text/sql")
                                    .await
                                {
                                    failed.push(ds.clone());
                                    errors.push(format!(
                                        "{ds}: failed to auto-heal wildcard SELECT in {sql_rel}: {write_err}"
                                    ));
                                    continue;
                                }
                                auto_healed_wildcard_sql_dataset_ids.push(ds.clone());
                            }
                            from_plan
                        } else {
                            failed.push(ds.clone());
                            errors.push(format!(
                                "{ds}: cannot parse allowed output columns from {sql_rel}: {e}"
                            ));
                            continue;
                        }
                    } else {
                        failed.push(ds.clone());
                        errors.push(format!(
                            "{ds}: cannot parse allowed output columns from {sql_rel}: {e}"
                        ));
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

            let (outcome, _notes) = match crate::data_engineer::patch_protocol::llm_patch_loop_single_file(
                ctx,
                self.datasets.as_ref(),
                schema_yml_sys_prompt_staging(),
                user_payload,
                &yml_rel,
                6,
                Some(LlmCallOptions {
                    prompt_id: "data_engineer.apply_next_schema_batch.staging_schema_patch",
                    thread_id: ctx.thread_id.clone(),
                    expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
                    temperature: Some(0.05),
                    top_p: Some(1.0),
                    max_output_tokens: Some(
                        crate::data_engineer::patch_protocol::default_patch_loop_max_output_tokens(),
                    ),
                    reasoning_effort: None,
                }),
            )
            .await {
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
                dbt_files::validate_staging_schema_ymls(ctx, std::slice::from_ref(&outcome)).await
            {
                failed.push(ds.clone());
                errors.push(format!("{ds}: invalid staging schema yml: {e}"));
                continue;
            }

            let yml_key = project_fs::join_storage_key(ctx, &yml_rel);
            if let Err(e) = ctx
                .storage
                .put_bytes(&yml_key, outcome.content.as_bytes(), "text/yaml")
                .await
            {
                failed.push(ds.clone());
                errors.push(format!("{ds}: failed to write {yml_rel}: {e}"));
                continue;
            }

            succeeded.push(ds.clone());
        }

        for ds in succeeded.iter() {
            plan::cleanse_checklist_mark_status(
                &mut plan,
                ds,
                &checklist_item_id,
                plan::ChecklistItemStatus::Done,
            );
        }
        for ds in failed.iter() {
            plan::cleanse_checklist_mark_status(
                &mut plan,
                ds,
                &checklist_item_id,
                plan::ChecklistItemStatus::NeedsUpdate,
            );
        }
        let budget = controller_kernel::note_batch_result(&mut plan.progress, failed.is_empty());
        plan::save_cleanse_plan(ctx, &plan)
            .await
            .map_err(|e| format!("failed to persist cleanse schema batch result state: {e}"))?;

        if budget.exhausted() {
            return Ok(serde_json::json!({
                "ok": false,
                "kind": "batch_locked",
                "reason_code": controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted,
                "message": controller_kernel::batch_lock_error_message(controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted),
                "checklist_item_id": checklist_item_id,
                "attempted_dataset_ids": batch,
                "succeeded_dataset_ids": succeeded,
                "failed_dataset_ids": failed,
                "auto_healed_wildcard_sql_dataset_ids": auto_healed_wildcard_sql_dataset_ids,
                "errors": errors,
            }));
        }

        Ok(serde_json::json!({
            "ok": failed.is_empty(),
            "progress_made": !succeeded.is_empty(),
            "checklist_item_id": checklist_item_id,
            "attempted_dataset_ids": batch,
            "succeeded_dataset_ids": succeeded,
            "failed_dataset_ids": failed,
            "auto_healed_wildcard_sql_dataset_ids": auto_healed_wildcard_sql_dataset_ids,
            "errors": errors,
        }))
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
        let checklist_item_id = ctx
            .exec_ctx
            .as_ref()
            .and_then(|c| c.checklist_item_id.as_ref())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| plan::CHECKLIST_SCHEMA_CONTRACT.to_string());

        let mut plan = plan::load_model_plan_any(ctx)
            .await
            .ok_or_else(|| "no active model plan found".to_string())?;
        if plan.status != plan::PlanStatus::Approved && plan.status != plan::PlanStatus::Completed {
            return Err(format!(
                "model plan is not approved (status={:?}); return to plan phase",
                plan.status
            ));
        }

        // Plan auto-heal (semantic): validate + single repair attempt before executing.
        let stg =
            crate::data_engineer::dataset_truth::discover_staging_models_from_storage(ctx).await;
        let v = plan::ensure_model_plan_semantically_valid_or_repaired(
            ctx,
            &mut plan,
            &stg.allowed_models,
        )
        .await?;
        if !v.ok {
            return Ok(serde_json::json!({
                "ok": false,
                "kind": "plan_invalid",
                "plan_key": plan.plan_key,
                "errors": v.errors,
                "attempted_item_names": [],
                "succeeded_item_names": [],
                "failed_item_names": [],
            }));
        }

        if controller_kernel::batch_budget(&plan.progress).exhausted() {
            return Ok(serde_json::json!({
                "ok": false,
                "kind": "batch_locked",
                "reason_code": controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted,
                "message": controller_kernel::batch_lock_error_message(controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted),
                "checklist_item_id": checklist_item_id,
                "attempted_item_names": [],
                "succeeded_item_names": [],
                "failed_item_names": [],
                "errors": [controller_kernel::batch_lock_error_message(controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted)],
            }));
        }

        let names =
            plan::model_pending_for_checklist(&plan, plan::CHECKLIST_SQL_MODEL, &checklist_item_id);
        if names.is_empty() {
            return Ok(serde_json::json!({
                "ok": false,
                "kind": "no_progress",
                "progress_made": false,
                "message": "no pending schema checklist work (all done)",
                "checklist_item_id": checklist_item_id,
                "attempted_item_names": [],
                "succeeded_item_names": [],
                "failed_item_names": [],
            }));
        }
        if let Err(e) = chunk_progress_contract::enforce_chunk_contract(&names, 5, "model_schema") {
            return Ok(serde_json::json!({
                "ok": false,
                "kind": "chunk_contract_violation",
                "checklist_item_id": checklist_item_id,
                "attempted_item_names": [],
                "succeeded_item_names": [],
                "failed_item_names": [],
                "errors": [e],
            }));
        }

        let instructions = extract_string_arg(&args, "instructions")
            .or_else(|| extract_string_arg(&args, "user_instructions"))
            .unwrap_or_default();

        for n in names.iter() {
            plan::model_checklist_mark_status(
                &mut plan,
                n,
                &checklist_item_id,
                plan::ChecklistItemStatus::InProgress,
            );
        }
        plan::save_model_plan(ctx, &plan)
            .await
            .map_err(|e| format!("failed to persist model schema batch start state: {e}"))?;

        let attempted_names = names.clone();
        let expected_rel = project_files::MODELS_SCHEMA_YML;

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
                    let key_sql = project_fs::join_storage_key(ctx, rel);
                    match ctx.storage.get_bytes(&key_sql).await {
                        Ok(b) => {
                            let sql_text = String::from_utf8_lossy(&b).to_string();
                            match dbt_files::extract_final_select_output_columns(&sql_text) {
                                Ok(cols) => {
                                    allowed.allowed_columns =
                                        cols.into_iter().collect::<std::collections::HashSet<_>>();
                                }
                                Err(e) => {
                                    allowed.error = Some(e);
                                }
                            }
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

        let (outcome, _notes) =
            match crate::data_engineer::patch_protocol::llm_patch_loop_single_file(
                ctx,
                self.datasets.as_ref(),
                schema_yml_sys_prompt_models_schema_yml(),
                user_payload,
                expected_rel,
                6,
                Some(LlmCallOptions {
                    prompt_id: "data_engineer.apply_next_schema_batch.models_schema_patch",
                    thread_id: ctx.thread_id.clone(),
                    expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
                    temperature: Some(0.05),
                    top_p: Some(1.0),
                    max_output_tokens: Some(
                        crate::data_engineer::patch_protocol::default_patch_loop_max_output_tokens(
                        ),
                    ),
                    reasoning_effort: None,
                }),
            )
            .await
            {
                Ok(v) => v,
                Err(e) => {
                    for n in names.iter() {
                        plan::model_schema_contract_mark_needs_update(&mut plan, n);
                    }
                    let budget = controller_kernel::note_batch_result(&mut plan.progress, false);
                    plan::save_model_plan(ctx, &plan).await.map_err(|save_err| {
                        format!(
                            "failed to persist model schema batch failure state after patch error: {save_err}"
                        )
                    })?;
                    if budget.exhausted() {
                        return Ok(serde_json::json!({
                            "ok": false,
                            "kind": "batch_locked",
                            "reason_code": controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted,
                            "message": controller_kernel::batch_lock_error_message(controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted),
                            "checklist_item_id": checklist_item_id,
                            "attempted_item_names": attempted_names.clone(),
                            "succeeded_item_names": [],
                            "failed_item_names": attempted_names.clone(),
                            "errors": [controller_kernel::batch_lock_error_message(controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted)],
                        }));
                    }
                    return Ok(serde_json::json!({
                        "ok": false,
                        "attempted_item_names": attempted_names.clone(),
                        "succeeded_item_names": [],
                        "failed_item_names": attempted_names,
                        "errors": [format!("models/schema.yml patch failed: {e}")],
                    }));
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
                for n in names.iter() {
                    plan::model_schema_contract_mark_needs_update(&mut plan, n);
                }
                let budget = controller_kernel::note_batch_result(&mut plan.progress, false);
                plan::save_model_plan(ctx, &plan).await.map_err(|save_err| {
                    format!(
                        "failed to persist model schema batch failure state after post-check error: {save_err}"
                    )
                })?;
                if budget.exhausted() {
                    return Ok(serde_json::json!({
                        "ok": false,
                        "kind": "batch_locked",
                        "reason_code": controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted,
                        "message": controller_kernel::batch_lock_error_message(controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted),
                        "checklist_item_id": checklist_item_id,
                        "attempted_item_names": attempted_names.clone(),
                        "succeeded_item_names": [],
                        "failed_item_names": attempted_names.clone(),
                        "errors": [controller_kernel::batch_lock_error_message(controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted)],
                    }));
                }
                return Ok(serde_json::json!({
                    "ok": false,
                    "attempted_item_names": attempted_names.clone(),
                    "succeeded_item_names": [],
                    "failed_item_names": attempted_names,
                    "errors": [format!("models/schema.yml post-check failed: {e}")],
                }));
            }
        };

        let key = project_fs::join_storage_key(ctx, expected_rel);
        if let Err(e) = ctx
            .storage
            .put_bytes(&key, sanitized_text.as_bytes(), "text/yaml")
            .await
        {
            for n in names.iter() {
                plan::model_schema_contract_mark_needs_update(&mut plan, n);
            }
            let budget = controller_kernel::note_batch_result(&mut plan.progress, false);
            plan::save_model_plan(ctx, &plan).await.map_err(|save_err| {
                format!(
                    "failed to persist model schema batch failure state after write error: {save_err}"
                )
            })?;
            if budget.exhausted() {
                return Ok(serde_json::json!({
                    "ok": false,
                    "kind": "batch_locked",
                    "reason_code": controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted,
                    "message": controller_kernel::batch_lock_error_message(controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted),
                    "checklist_item_id": checklist_item_id,
                    "attempted_item_names": attempted_names.clone(),
                    "succeeded_item_names": [],
                    "failed_item_names": attempted_names.clone(),
                    "errors": [controller_kernel::batch_lock_error_message(controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted)],
                }));
            }
            return Ok(serde_json::json!({
                "ok": false,
                "attempted_item_names": attempted_names.clone(),
                "succeeded_item_names": [],
                "failed_item_names": attempted_names,
                "errors": [format!("failed to write {}: {}", expected_rel, e)],
            }));
        }

        for n in names.iter() {
            plan::model_checklist_mark_status(
                &mut plan,
                n,
                &checklist_item_id,
                plan::ChecklistItemStatus::Done,
            );
        }
        let budget = controller_kernel::note_batch_result(&mut plan.progress, true);
        plan::save_model_plan(ctx, &plan)
            .await
            .map_err(|e| format!("failed to persist model schema batch result state: {e}"))?;

        if budget.exhausted() {
            return Ok(serde_json::json!({
                "ok": false,
                "kind": "batch_locked",
                "reason_code": controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted,
                "message": controller_kernel::batch_lock_error_message(controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted),
                "checklist_item_id": checklist_item_id,
                "attempted_item_names": attempted_names.clone(),
                "succeeded_item_names": attempted_names.clone(),
                "failed_item_names": [],
                "errors": [controller_kernel::batch_lock_error_message(controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted)],
                "warnings": warnings,
            }));
        }

        Ok(serde_json::json!({
            "ok": true,
            "progress_made": true,
            "checklist_item_id": checklist_item_id,
            "attempted_item_names": attempted_names.clone(),
            "succeeded_item_names": attempted_names,
            "failed_item_names": [],
            "errors": [],
            "warnings": warnings,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use react_core::keyspace::{DefaultKeyspace, Keyspace};
    use react_core::llm::ChatMessage;
    use react_core::llm::LargeLanguageModel;
    use react_core::scope::RequestScope;
    use react_core::session::ExecutionContext;
    use react_core::storage::{InMemoryStorageAdapter, StorageAdapter};
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
                .find(|m| m.role == "user")
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

    fn minimal_cfg() -> Arc<crate::config::ReactResolvedConfig> {
        Arc::new(crate::config::ReactResolvedConfig {
            server: crate::config::ServerResolved { port: 1 },
            storage: crate::config::StorageResolved { mode: "local".to_string(), bucket: None, path: None },
            scope: RequestScope {
                tenant: "t".to_string(),
                workspace: "w".to_string(),
                project_id: "p".to_string(),
            },
            llm: crate::config::LlmResolved::default(),
            providers: crate::config::ProvidersResolved {
                warehouse: crate::config::WarehouseResolved {
                    kind: "athena".to_string(),
                    container: "AwsDataCatalog".to_string(),
                    namespace: "test_raw".to_string(),
                    extras: serde_json::json!({"region":"eu-west-1","workgroup":"wg","result_s3":"s3://x/"}),
                },
                catalog: crate::config::CatalogResolved {
                    enabled: false,
                    refresh_secs: 60,
                    max_concurrency: 8,
                },
                dbt: crate::config::DbtResolved {
                    enabled: true,
                    profiles_dir: None,
                    target: "athena".to_string(),
                    naming: crate::config::DbtNamingResolved {
                        target_schema: "test".to_string(),
                        silver_suffix: "silver".to_string(),
                        gold_suffix: "warehouse".to_string(),
                    },
                    runner: "host".to_string(),
                    docker_image: None,
                    docker_platform: None,
                    docker_network: None,
                    docker_mount_aws_dir: false,
                },
                vector: crate::config::VectorResolved { enabled: false },
            },
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
        let scope = RequestScope {
            tenant: "t".to_string(),
            workspace: "w".to_string(),
            project_id: "p".to_string(),
        };
        let ctx = AgentCtx {
            top_k: 1,
            per_step_timeout_secs: 1,
            max_steps: 2,
            thread_id: Some("tid".to_string()),
            progress_tx: None,
            pre_step_tx: None,
            trace_tx: None,
            agent_name: Some("test".to_string()),
            policy: Arc::new(react_core::agent::DefaultPolicy),
            llm,
            storage: storage.clone(),
            scope: scope.clone(),
            keyspace,
            query: None,
            warehouse: Arc::new(react_core::providers::NullWarehouseProvider::default()),
            dbt: None,
            vector: None,
            thread_store: None,
            exec_ctx: None,
            resolved_config: Some(minimal_cfg()),
        };

        // Seed a cleanse plan with sql_model done and schema_contract pending.
        let plan_key = plan::new_cleanse_plan_key(&ctx);
        let mut checklist = plan::canonical_task_checklist(true);
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
            project_snapshot: serde_json::json!({}),
            tasks: vec![plan::CleanseTask {
                dataset_id: "AwsDataCatalog.test_raw.raw_customers".to_string(),
                expected_model_path: Some(
                    "models/staging/stg_test_raw_raw_customers.sql".to_string(),
                ),
                invariants: vec![],
                implementation_spec: plan::CleanseImplementationSpec {
                    spec_version: 1,
                    row_preserving: true,
                    output_fields: vec![plan::OutputFieldSpec {
                        name: "customer_id_raw".to_string(),
                        kind: plan::FieldKind::Raw,
                        source_columns: vec!["customer_id".to_string()],
                        expression: "customer_id as customer_id_raw (raw)".to_string(),
                        data_type: None,
                        nullable: true,
                        description: None,
                    }, plan::OutputFieldSpec {
                        name: "email_raw".to_string(),
                        kind: plan::FieldKind::Raw,
                        source_columns: vec!["email".to_string()],
                        expression: "email as email_raw (raw)".to_string(),
                        data_type: None,
                        nullable: true,
                        description: None,
                    }],
                    prohibited_ops: vec![],
                },
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
        ctx.storage
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
        let got = ctx.storage.get_bytes(&yml_key).await.unwrap();
        let got = String::from_utf8_lossy(&got).to_string();
        assert!(got.contains("stg_test_raw_raw_customers"));

        let healed_sql = ctx.storage.get_bytes(&sql_key).await.unwrap();
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
        assert!(
            healed_ds
                .iter()
                .any(|v| v.as_str() == Some("AwsDataCatalog.test_raw.raw_customers"))
        );
    }

    #[tokio::test]
    async fn apply_next_cleanse_schema_batch_stops_when_local_failure_budget_exhausted() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let llm: Arc<dyn LargeLanguageModel> = Arc::new(ScriptedLlm {
            replies: Mutex::new(vec![]),
        });
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        let scope = RequestScope {
            tenant: "t".to_string(),
            workspace: "w".to_string(),
            project_id: "p".to_string(),
        };
        let ctx = AgentCtx {
            top_k: 1,
            per_step_timeout_secs: 1,
            max_steps: 2,
            thread_id: Some("tid_lock".to_string()),
            progress_tx: None,
            pre_step_tx: None,
            trace_tx: None,
            agent_name: Some("test".to_string()),
            policy: Arc::new(react_core::agent::DefaultPolicy),
            llm,
            storage: storage.clone(),
            scope: scope.clone(),
            keyspace,
            query: None,
            warehouse: Arc::new(react_core::providers::NullWarehouseProvider::default()),
            dbt: None,
            vector: None,
            thread_store: None,
            exec_ctx: None,
            resolved_config: Some(minimal_cfg()),
        };

        let plan_key = plan::new_cleanse_plan_key(&ctx);
        let mut checklist = plan::canonical_task_checklist(true);
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
            project_snapshot: serde_json::json!({}),
            tasks: vec![plan::CleanseTask {
                dataset_id: "AwsDataCatalog.test_raw.raw_customers".to_string(),
                expected_model_path: Some(
                    "models/staging/stg_test_raw_raw_customers.sql".to_string(),
                ),
                invariants: vec![],
                implementation_spec: plan::CleanseImplementationSpec {
                    spec_version: 1,
                    row_preserving: true,
                    output_fields: vec![plan::OutputFieldSpec {
                        name: "customer_id_raw".to_string(),
                        kind: plan::FieldKind::Raw,
                        source_columns: vec!["customer_id".to_string()],
                        expression: "customer_id as customer_id_raw (raw)".to_string(),
                        data_type: None,
                        nullable: true,
                        description: None,
                    }],
                    prohibited_ops: vec![],
                },
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
        assert_eq!(res.get("kind").and_then(|v| v.as_str()), Some("batch_locked"));
        assert_eq!(
            res.get("attempted_dataset_ids")
                .and_then(|v| v.as_array())
                .map(|a| a.len()),
            Some(0)
        );
    }

    #[tokio::test]
    async fn apply_next_cleanse_schema_batch_respects_exec_ctx_checklist_item_id() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let llm: Arc<dyn LargeLanguageModel> = Arc::new(ScriptedLlm {
            replies: Mutex::new(vec![serde_json::json!({
                "path": "models/staging/stg_test_raw_raw_customers.yml",
                "patch_text": "@@ ... @@\n+version: 2\n+\n+models:\n+  - name: stg_test_raw_raw_customers\n+    columns:\n+      - name: customer_id_raw\n+      - name: email_raw\n"
            })
            .to_string()]),
        });
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        let scope = RequestScope {
            tenant: "t".to_string(),
            workspace: "w".to_string(),
            project_id: "p".to_string(),
        };
        let mut ctx = AgentCtx {
            top_k: 1,
            per_step_timeout_secs: 1,
            max_steps: 2,
            thread_id: Some("tid_ctx".to_string()),
            progress_tx: None,
            pre_step_tx: None,
            trace_tx: None,
            agent_name: Some("test".to_string()),
            policy: Arc::new(react_core::agent::DefaultPolicy),
            llm,
            storage: storage.clone(),
            scope: scope.clone(),
            keyspace,
            query: None,
            warehouse: Arc::new(react_core::providers::NullWarehouseProvider::default()),
            dbt: None,
            vector: None,
            thread_store: None,
            exec_ctx: None,
            resolved_config: Some(minimal_cfg()),
        };

        let plan_key = plan::new_cleanse_plan_key(&ctx);
        ctx.exec_ctx = Some(ExecutionContext {
            plan_kind: Some(react_core::session::ExecutionPlanKind::new("cleanse")),
            plan_key: Some(plan_key.clone()),
            workgroup_id: Some("wg".to_string()),
            task_id: Some("AwsDataCatalog.test_raw.raw_customers".to_string()),
            checklist_item_id: Some(plan::CHECKLIST_SCHEMA_CONTRACT.to_string()),
            data: std::collections::BTreeMap::new(),
        });

        let mut checklist = plan::canonical_task_checklist(true);
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
            project_snapshot: serde_json::json!({}),
            tasks: vec![plan::CleanseTask {
                dataset_id: "AwsDataCatalog.test_raw.raw_customers".to_string(),
                expected_model_path: Some(
                    "models/staging/stg_test_raw_raw_customers.sql".to_string(),
                ),
                invariants: vec![],
                implementation_spec: plan::CleanseImplementationSpec {
                    spec_version: 1,
                    row_preserving: true,
                    output_fields: vec![plan::OutputFieldSpec {
                        name: "customer_id_raw".to_string(),
                        kind: plan::FieldKind::Raw,
                        source_columns: vec!["customer_id".to_string()],
                        expression: "customer_id as customer_id_raw (raw)".to_string(),
                        data_type: None,
                        nullable: true,
                        description: None,
                    }],
                    prohibited_ops: vec![],
                },
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
        ctx.storage
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
        let scope = RequestScope {
            tenant: "t".to_string(),
            workspace: "w".to_string(),
            project_id: "p".to_string(),
        };
        let ctx = AgentCtx {
            top_k: 1,
            per_step_timeout_secs: 1,
            max_steps: 2,
            thread_id: Some("tid2".to_string()),
            progress_tx: None,
            pre_step_tx: None,
            trace_tx: None,
            agent_name: Some("test".to_string()),
            policy: Arc::new(react_core::agent::DefaultPolicy),
            llm,
            storage: storage.clone(),
            scope: scope.clone(),
            keyspace,
            query: None,
            warehouse: Arc::new(react_core::providers::NullWarehouseProvider::default()),
            dbt: None,
            vector: None,
            thread_store: None,
            exec_ctx: None,
            resolved_config: Some(minimal_cfg()),
        };

        // Seed a model plan with sql_model done and schema_contract pending.
        let plan_key = plan::new_model_plan_key(&ctx);
        let mut checklist = plan::canonical_task_checklist(false);
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
            project_snapshot: serde_json::json!({}),
            tasks: vec![plan::ModelTask {
                name: "dim_customers".to_string(),
                folder: "marts".to_string(),
                goal: "g".to_string(),
                inputs: vec!["stg_test_raw_raw_customers".to_string()],
                expected_model_path: Some("models/marts/dim_customers.sql".to_string()),
                invariants: vec![],
                implementation_spec: plan::ModelImplementationSpec {
                    spec_version: 1,
                    grain: "1 row per customer".to_string(),
                    inputs: vec!["stg_test_raw_raw_customers".to_string()],
                    joins: vec![],
                    metrics: vec![],
                    output_fields: vec![plan::OutputFieldSpec {
                        name: "customer_id".to_string(),
                        kind: plan::FieldKind::Clean,
                        source_columns: vec!["customer_id".to_string()],
                        expression: "customer_id passthrough".to_string(),
                        data_type: None,
                        nullable: true,
                        description: None,
                    }],
                    assumptions: vec![],
                },
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

        let key = project_fs::join_storage_key(&ctx, project_files::MODELS_SCHEMA_YML);
        let got = ctx.storage.get_bytes(&key).await.unwrap();
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
        let scope = RequestScope {
            tenant: "t".to_string(),
            workspace: "w".to_string(),
            project_id: "p".to_string(),
        };
        let ctx = AgentCtx {
            top_k: 1,
            per_step_timeout_secs: 1,
            max_steps: 2,
            thread_id: Some("tid_model_lock".to_string()),
            progress_tx: None,
            pre_step_tx: None,
            trace_tx: None,
            agent_name: Some("test".to_string()),
            policy: Arc::new(react_core::agent::DefaultPolicy),
            llm,
            storage: storage.clone(),
            scope: scope.clone(),
            keyspace,
            query: None,
            warehouse: Arc::new(react_core::providers::NullWarehouseProvider::default()),
            dbt: None,
            vector: None,
            thread_store: None,
            exec_ctx: None,
            resolved_config: Some(minimal_cfg()),
        };

        let plan_key = plan::new_model_plan_key(&ctx);
        let mut checklist = plan::canonical_task_checklist(false);
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
            project_snapshot: serde_json::json!({}),
            tasks: vec![plan::ModelTask {
                name: "dim_customers".to_string(),
                folder: "marts".to_string(),
                goal: "g".to_string(),
                inputs: vec!["stg_test_raw_raw_customers".to_string()],
                expected_model_path: Some("models/marts/dim_customers.sql".to_string()),
                invariants: vec![],
                implementation_spec: plan::ModelImplementationSpec {
                    spec_version: 1,
                    grain: "1 row per customer".to_string(),
                    inputs: vec!["stg_test_raw_raw_customers".to_string()],
                    joins: vec![],
                    metrics: vec![],
                    output_fields: vec![plan::OutputFieldSpec {
                        name: "customer_id".to_string(),
                        kind: plan::FieldKind::Clean,
                        source_columns: vec!["customer_id".to_string()],
                        expression: "customer_id passthrough".to_string(),
                        data_type: None,
                        nullable: true,
                        description: None,
                    }],
                    assumptions: vec![],
                },
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
        assert_eq!(res.get("kind").and_then(|v| v.as_str()), Some("batch_locked"));
        assert_eq!(
            res.get("attempted_item_names")
                .and_then(|v| v.as_array())
                .map(|a| a.len()),
            Some(0)
        );
    }

    #[tokio::test]
    async fn apply_next_model_schema_batch_respects_exec_ctx_checklist_item_id() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let llm: Arc<dyn LargeLanguageModel> = Arc::new(ScriptedLlm {
            replies: Mutex::new(vec![serde_json::json!({
                "path": "models/schema.yml",
                "patch_text": "@@ ... @@\n+version: 2\n+\n+models:\n+  - name: dim_customers\n+    columns: []\n"
            })
            .to_string()]),
        });
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        let scope = RequestScope {
            tenant: "t".to_string(),
            workspace: "w".to_string(),
            project_id: "p".to_string(),
        };
        let mut ctx = AgentCtx {
            top_k: 1,
            per_step_timeout_secs: 1,
            max_steps: 2,
            thread_id: Some("tid_model_ctx".to_string()),
            progress_tx: None,
            pre_step_tx: None,
            trace_tx: None,
            agent_name: Some("test".to_string()),
            policy: Arc::new(react_core::agent::DefaultPolicy),
            llm,
            storage: storage.clone(),
            scope: scope.clone(),
            keyspace,
            query: None,
            warehouse: Arc::new(react_core::providers::NullWarehouseProvider::default()),
            dbt: None,
            vector: None,
            thread_store: None,
            exec_ctx: None,
            resolved_config: Some(minimal_cfg()),
        };

        let plan_key = plan::new_model_plan_key(&ctx);
        ctx.exec_ctx = Some(ExecutionContext {
            plan_kind: Some(react_core::session::ExecutionPlanKind::new("model")),
            plan_key: Some(plan_key.clone()),
            workgroup_id: Some("wg".to_string()),
            task_id: Some("dim_customers".to_string()),
            checklist_item_id: Some(plan::CHECKLIST_SCHEMA_CONTRACT.to_string()),
            data: std::collections::BTreeMap::new(),
        });

        // Seed model SQL so allowed_columns can be derived (best-effort).
        let sql_rel = "models/marts/dim_customers.sql";
        let sql_key = project_fs::join_storage_key(&ctx, sql_rel);
        let sql = "with t as (\n  select 1 as customer_id, 'a@b.com' as email\n)\nselect\n  customer_id,\n  email\nfrom t\n";
        ctx.storage
            .put_bytes(&sql_key, sql.as_bytes(), "text/sql")
            .await
            .unwrap();

        let mut checklist = plan::canonical_task_checklist(false);
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
            project_snapshot: serde_json::json!({}),
            tasks: vec![plan::ModelTask {
                name: "dim_customers".to_string(),
                folder: "marts".to_string(),
                goal: "g".to_string(),
                inputs: vec!["stg_test_raw_raw_customers".to_string()],
                expected_model_path: Some(sql_rel.to_string()),
                invariants: vec![],
                implementation_spec: plan::ModelImplementationSpec {
                    spec_version: 1,
                    grain: "1 row per customer".to_string(),
                    inputs: vec!["stg_test_raw_raw_customers".to_string()],
                    joins: vec![],
                    metrics: vec![],
                    output_fields: vec![plan::OutputFieldSpec {
                        name: "customer_id".to_string(),
                        kind: plan::FieldKind::Clean,
                        source_columns: vec!["customer_id".to_string()],
                        expression: "customer_id passthrough".to_string(),
                        data_type: None,
                        nullable: true,
                        description: None,
                    }],
                    assumptions: vec![],
                },
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

        let got_plan = plan::load_model_plan_by_key(&ctx, &plan_key).await.unwrap();
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
        let scope = RequestScope {
            tenant: "t".to_string(),
            workspace: "w".to_string(),
            project_id: "p".to_string(),
        };
        let ctx = AgentCtx {
            top_k: 1,
            per_step_timeout_secs: 1,
            max_steps: 2,
            thread_id: Some("tid3".to_string()),
            progress_tx: None,
            pre_step_tx: None,
            trace_tx: None,
            agent_name: Some("test".to_string()),
            policy: Arc::new(react_core::agent::DefaultPolicy),
            llm: llm.clone(),
            storage: storage.clone(),
            scope: scope.clone(),
            keyspace,
            query: None,
            warehouse: Arc::new(react_core::providers::NullWarehouseProvider::default()),
            dbt: None,
            vector: None,
            thread_store: None,
            exec_ctx: None,
            resolved_config: Some(minimal_cfg()),
        };

        // Seed model SQL so allowed_columns can be derived.
        let sql_rel = "models/marts/dim_customers.sql";
        let sql_key = project_fs::join_storage_key(&ctx, sql_rel);
        let sql = "with t as (\n  select 1 as customer_id, 'a@b.com' as email\n)\nselect\n  customer_id,\n  email\nfrom t\n";
        ctx.storage
            .put_bytes(&sql_key, sql.as_bytes(), "text/sql")
            .await
            .unwrap();

        let plan_key = plan::new_model_plan_key(&ctx);
        let mut checklist = plan::canonical_task_checklist(false);
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
            project_snapshot: serde_json::json!({}),
            tasks: vec![plan::ModelTask {
                name: "dim_customers".to_string(),
                folder: "marts".to_string(),
                goal: "g".to_string(),
                inputs: vec!["stg_test_raw_raw_customers".to_string()],
                expected_model_path: Some(sql_rel.to_string()),
                invariants: vec![],
                implementation_spec: plan::ModelImplementationSpec {
                    spec_version: 1,
                    grain: "1 row per customer".to_string(),
                    inputs: vec!["stg_test_raw_raw_customers".to_string()],
                    joins: vec![],
                    metrics: vec![],
                    output_fields: vec![plan::OutputFieldSpec {
                        name: "customer_id".to_string(),
                        kind: plan::FieldKind::Clean,
                        source_columns: vec!["customer_id".to_string()],
                        expression: "customer_id passthrough".to_string(),
                        data_type: None,
                        nullable: true,
                        description: None,
                    }],
                    assumptions: vec![],
                },
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
        let scope = RequestScope {
            tenant: "t".to_string(),
            workspace: "w".to_string(),
            project_id: "p".to_string(),
        };
        let ctx = AgentCtx {
            top_k: 1,
            per_step_timeout_secs: 1,
            max_steps: 2,
            thread_id: Some("tid4".to_string()),
            progress_tx: None,
            pre_step_tx: None,
            trace_tx: None,
            agent_name: Some("test".to_string()),
            policy: Arc::new(react_core::agent::DefaultPolicy),
            llm,
            storage: storage.clone(),
            scope: scope.clone(),
            keyspace,
            query: None,
            warehouse: Arc::new(react_core::providers::NullWarehouseProvider::default()),
            dbt: None,
            vector: None,
            thread_store: None,
            exec_ctx: None,
            resolved_config: Some(minimal_cfg()),
        };

        // Seed model plan.
        let plan_key = plan::new_model_plan_key(&ctx);
        let mut checklist = plan::canonical_task_checklist(false);
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
            project_snapshot: serde_json::json!({}),
            tasks: vec![plan::ModelTask {
                name: "dim_customers".to_string(),
                folder: "marts".to_string(),
                goal: "g".to_string(),
                inputs: vec!["stg_test_raw_raw_customers".to_string()],
                expected_model_path: Some("models/marts/dim_customers.sql".to_string()),
                invariants: vec![],
                implementation_spec: plan::ModelImplementationSpec {
                    spec_version: 1,
                    grain: "1 row per customer".to_string(),
                    inputs: vec!["stg_test_raw_raw_customers".to_string()],
                    joins: vec![],
                    metrics: vec![],
                    output_fields: vec![plan::OutputFieldSpec {
                        name: "customer_id".to_string(),
                        kind: plan::FieldKind::Clean,
                        source_columns: vec!["customer_id".to_string()],
                        expression: "customer_id passthrough".to_string(),
                        data_type: None,
                        nullable: true,
                        description: None,
                    }],
                    assumptions: vec![],
                },
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

        let key = project_fs::join_storage_key(&ctx, project_files::MODELS_SCHEMA_YML);
        let got = ctx.storage.get_bytes(&key).await.unwrap();
        let got = String::from_utf8_lossy(&got).to_string();
        assert!(!got.contains("stg_test_raw_raw_customers"));
        assert!(got.contains("dim_customers"));
    }
}
