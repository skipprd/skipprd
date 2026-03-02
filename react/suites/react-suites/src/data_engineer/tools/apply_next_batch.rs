use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;

use react_core::agent::AgentCtx;
use react_core::providers::DatasetCatalogProvider;
use react_core::tools::Tool;

use crate::data_engineer::dataset_truth;
use crate::data_engineer::chunk_progress_contract;
use crate::data_engineer::controller_kernel;
use crate::data_engineer::plan;
use crate::data_engineer::plan::{CleansePlan, ModelPlan};
use crate::data_engineer::progress_controller::{
    BatchFailureKind, DataEngineerEvent, ExecutionTier, FailedModelRef,
};
use crate::data_engineer::state_manager;
use crate::data_engineer::tools;

fn extract_string_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|x| x.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn mark_in_progress_cleanse(plan: &mut CleansePlan, dataset_ids: &[String]) {
    for ds in dataset_ids.iter() {
        plan::cleanse_mark_in_progress(plan, ds);
    }
}

fn mark_in_progress_cleanse_checklist(
    plan: &mut CleansePlan,
    dataset_ids: &[String],
    checklist: &str,
) {
    for ds in dataset_ids.iter() {
        plan::cleanse_checklist_mark_status(
            plan,
            ds,
            checklist,
            plan::ChecklistItemStatus::InProgress,
        );
    }
}

fn mark_in_progress_model(plan: &mut ModelPlan, names: &[String]) {
    for n in names.iter() {
        plan::model_mark_in_progress(plan, n);
    }
}

fn mark_in_progress_model_checklist(plan: &mut ModelPlan, names: &[String], checklist: &str) {
    for n in names.iter() {
        plan::model_checklist_mark_status(
            plan,
            n,
            checklist,
            plan::ChecklistItemStatus::InProgress,
        );
    }
}

fn mark_needs_update_cleanse(plan: &mut CleansePlan, dataset_ids: &[String], note: &str) {
    let _ = note; // errors are surfaced via tool output; plan tracks needs_update
    for ds in dataset_ids.iter() {
        plan::cleanse_mark_needs_update(plan, ds);
    }
}

fn mark_needs_update_model(plan: &mut ModelPlan, names: &[String], note: &str) {
    let _ = note; // errors are surfaced via tool output; plan tracks needs_update
    for n in names.iter() {
        plan::model_mark_needs_update(plan, n);
    }
}

fn sql_model_checklist_status(items: &[plan::PlanChecklistItem]) -> plan::ChecklistItemStatus {
    items
        .iter()
        .find(|it| it.checklist_item_id == "sql_model")
        .map(|it| it.status)
        .unwrap_or(plan::ChecklistItemStatus::Pending)
}

fn classify_batch_failure_kind(errors: &[String]) -> BatchFailureKind {
    let joined = errors.join("\n").to_ascii_lowercase();
    if joined.contains("sql validation failed")
        || joined.contains("column_not_found")
        || joined.contains("compilation error")
        || joined.contains("runtime error")
    {
        return BatchFailureKind::SqlValidation;
    }
    if joined.contains("schema")
        || joined.contains("contract")
        || joined.contains("yaml")
        || joined.contains("parse")
    {
        return BatchFailureKind::SchemaOrContract;
    }
    if joined.contains("service error")
        || joined.contains("timeout")
        || joined.contains("throttle")
        || joined.contains("temporar")
    {
        return BatchFailureKind::InfraTransient;
    }
    BatchFailureKind::Unknown
}

async fn emit_batch_event(ctx: &AgentCtx, event: DataEngineerEvent) -> Result<(), String> {
    let Some(thread_store) = ctx.thread_store.as_ref() else {
        return Ok(());
    };
    let Some(thread_id) = ctx.thread_id.as_deref() else {
        return Ok(());
    };
    state_manager::apply_execution_event(thread_store, thread_id, event)
        .await
        .map(|_| ())
}

#[derive(Clone)]
pub struct ApplyNextCleanseBatchTool {
    pub datasets: Option<Arc<dyn DatasetCatalogProvider>>,
}

#[async_trait]
impl Tool for ApplyNextCleanseBatchTool {
    fn name(&self) -> &'static str {
        "apply_next_cleanse_batch"
    }

    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
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
                "errors": [controller_kernel::batch_lock_error_message(controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted)],
                "attempted_dataset_ids": [],
                "succeeded_dataset_ids": [],
                "failed_dataset_ids": [],
            }));
        }

        // Keep batch selection consistent with the suite driver via shared next-action resolution.
        let next_action = plan::cleanse_next_authoring_action(&plan);
        let batch = next_action.author_sql_ids();
        if batch.is_empty() {
            if let plan::AuthoringNextAction::AuthorSchema(ids) = &next_action {
                return Ok(serde_json::json!({
                    "ok": true,
                    "kind": "defer_schema_batch",
                    "message": "next deterministic action is schema checklist work; call apply_next_cleanse_schema_batch",
                    "pending_schema_contract_dataset_ids": ids,
                    "attempted_dataset_ids": [],
                    "succeeded_dataset_ids": [],
                    "failed_dataset_ids": [],
                    "errors": [],
                }));
            }
            if matches!(next_action, plan::AuthoringNextAction::Validate) {
                return Ok(serde_json::json!({
                    "ok": true,
                    "kind": "defer_validate",
                    "message": "next deterministic action is validate; transition to cleanse_validate",
                    "attempted_dataset_ids": [],
                    "succeeded_dataset_ids": [],
                    "failed_dataset_ids": [],
                    "errors": [],
                }));
            }
            let pending_schema = plan::cleanse_pending_schema_contracts(&plan);
            if !pending_schema.is_empty() {
                return Ok(serde_json::json!({
                    "ok": true,
                    "message": "no remaining cleanse SQL tasks; schema contracts pending",
                    "done": false,
                    "pending_schema_contract_dataset_ids": pending_schema,
                    "attempted_dataset_ids": [],
                    "succeeded_dataset_ids": [],
                    "failed_dataset_ids": [],
                }));
            }
            let done = plan::cleanse_all_done(&plan);
            if done {}
            if !done {
                let mut blocked: Vec<String> = Vec::new();
                for t in plan.tasks.iter() {
                    let st = sql_model_checklist_status(&t.checklist);
                    if matches!(st, plan::ChecklistItemStatus::Blocked) {
                        blocked.push(t.dataset_id.clone());
                    }
                }
                blocked.sort();
                blocked.dedup();
                let msg = "no runnable cleanse SQL tasks remain, but plan is not complete (blocked tasks exist)";
                return Ok(serde_json::json!({
                    "ok": false,
                    "kind": "plan_blocked",
                    "plan_key": plan.plan_key,
                    "message": msg,
                    "errors": [msg],
                    "blocked_dataset_ids": blocked,
                    "attempted_dataset_ids": [],
                    "succeeded_dataset_ids": [],
                    "failed_dataset_ids": [],
                }));
            }
            return Ok(serde_json::json!({
                "ok": true,
                "message": "no remaining cleanse tasks in next batch (all done)",
                "done": done,
                "pending_schema_contract_dataset_ids": pending_schema,
                "attempted_dataset_ids": [],
                "succeeded_dataset_ids": [],
                "failed_dataset_ids": [],
            }));
        }
        if let Err(e) = chunk_progress_contract::enforce_chunk_contract(&batch, 5, "cleanse_sql") {
            return Ok(serde_json::json!({
                "ok": false,
                "kind": "chunk_contract_violation",
                "errors": [e],
                "attempted_dataset_ids": [],
                "succeeded_dataset_ids": [],
                "failed_dataset_ids": [],
            }));
        }

        // Truth gating (fail-fast): only proceed if schema() proves each dataset exists.
        if let Some(q) = ctx.query.as_ref() {
            let mut gating_errors: Vec<String> = Vec::new();
            for ds in batch.iter() {
                if let Err(e) = q.schema(ds).await {
                    gating_errors.push(format!(
                        "{}: schema lookup failed (treating as fact): {}",
                        ds, e
                    ));
                }
            }
            if !gating_errors.is_empty() {
                mark_needs_update_cleanse(
                    &mut plan,
                    &batch,
                    "dataset schema lookup failed; dataset not usable",
                );
                controller_kernel::note_batch_result(&mut plan.progress, false);
                plan::save_cleanse_plan(ctx, &plan)
                    .await
                    .map_err(|e| format!("failed to save cleanse plan after schema gating failure: {e}"))?;
                return Ok(serde_json::json!({
                    "ok": false,
                    "attempted_dataset_ids": batch.clone(),
                    "succeeded_dataset_ids": [],
                    "failed_dataset_ids": batch.clone(),
                    "errors": gating_errors,
                }));
            }
        }

        // Optional user instruction passthrough to inner tool.
        let instructions = extract_string_arg(&args, "instructions")
            .or_else(|| extract_string_arg(&args, "user_instructions"));

        // Use explicit exec context (workgroup/checklist) when present so we can mark the correct
        // checklist item complete and avoid infinite loops on secondary SQL checklist items.
        let checklist_item_id = ctx
            .exec_ctx
            .as_ref()
            .and_then(|c| c.checklist_item_id.as_ref())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| plan::CHECKLIST_SQL_MODEL.to_string());

        // Mark batch in progress before running (so plan snapshot reflects active remediation).
        if checklist_item_id == plan::CHECKLIST_SQL_MODEL {
            mark_in_progress_cleanse(&mut plan, &batch);
        } else {
            mark_in_progress_cleanse_checklist(&mut plan, &batch, &checklist_item_id);
        }
        plan::save_cleanse_plan(ctx, &plan)
            .await
            .map_err(|e| format!("failed to save cleanse plan after marking in-progress: {e}"))?;

        let mut inner_args = serde_json::json!({ "dataset_ids": batch.clone() });
        if let Some(i) = instructions {
            inner_args["instructions"] = Value::String(i);
        }

        let inner = tools::staging_model::StagingModelTool {
            datasets: self.datasets.clone(),
        };
        let res = match inner.call(inner_args, ctx).await {
            Ok(v) => v,
            Err(e) => {
                // Tool error: mark whole batch as needs_update and return a structured failure (not an Err),
                // so callers can render it and retry deterministically.
                mark_needs_update_cleanse(
                    &mut plan,
                    &batch,
                    &format!("apply_next_cleanse_batch failed: {}", e.trim()),
                );
                controller_kernel::note_batch_result(&mut plan.progress, false);
                plan::save_cleanse_plan(ctx, &plan)
                    .await
                    .map_err(|e| format!("failed to save cleanse plan after batch tool failure: {e}"))?;
                return Ok(serde_json::json!({
                    "ok": false,
                    "attempted_dataset_ids": batch,
                    "succeeded_dataset_ids": [],
                    "failed_dataset_ids": batch,
                    "errors": [e],
                }));
            }
        };

        // Normalize the result shape we emit so plan progress derivation can be deterministic.
        let ok = res.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        let succeeded: Vec<String> = res
            .get("succeeded_dataset_ids")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let attempted: Vec<String> = batch.clone();

        // Compute failed as attempted - succeeded.
        let succ_set: HashSet<String> = succeeded.iter().cloned().collect();
        let failed: Vec<String> = attempted
            .iter()
            .filter(|ds| !succ_set.contains(*ds))
            .cloned()
            .collect();

        // Update task statuses.
        for ds in succeeded.iter() {
            if checklist_item_id == plan::CHECKLIST_SQL_MODEL {
                plan::cleanse_mark_done(&mut plan, ds);
            } else {
                plan::cleanse_checklist_mark_status(
                    &mut plan,
                    ds,
                    &checklist_item_id,
                    plan::ChecklistItemStatus::Done,
                );
            }
        }
        if !failed.is_empty() || !ok {
            let err = res
                .get("errors")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .unwrap_or("batch failed");
            let note = format!("apply_next_cleanse_batch failed: {}", err.trim());
            let _ = note;
            for ds in failed.iter() {
                if checklist_item_id == plan::CHECKLIST_SQL_MODEL {
                    plan::cleanse_mark_needs_update(&mut plan, ds);
                } else {
                    plan::cleanse_checklist_mark_status(
                        &mut plan,
                        ds,
                        &checklist_item_id,
                        plan::ChecklistItemStatus::NeedsUpdate,
                    );
                }
            }
            let mut failed_targets: Vec<FailedModelRef> = Vec::new();
            for ds in failed.iter() {
                let expected_path = plan
                    .tasks
                    .iter()
                    .find(|t| t.dataset_id == *ds)
                    .and_then(|t| t.expected_model_path.clone());
                failed_targets.push(FailedModelRef {
                    name: ds.clone(),
                    file: expected_path.unwrap_or_default(),
                });
            }
            let errors: Vec<String> = res
                .get("errors")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let kind = classify_batch_failure_kind(&errors);
            let brief = if err.trim().is_empty() {
                "apply_next_cleanse_batch failed".to_string()
            } else {
                err.to_string()
            };
            emit_batch_event(
                ctx,
                DataEngineerEvent::BatchAuthoringFailed {
                    tier: ExecutionTier::Cleanse,
                    kind,
                    failed_targets,
                    brief,
                },
            )
            .await?;
        } else {
            emit_batch_event(
                ctx,
                DataEngineerEvent::BatchAuthoringRecovered,
            )
            .await?;
        }
        let budget = controller_kernel::note_batch_result(&mut plan.progress, ok && failed.is_empty());
        plan::save_cleanse_plan(ctx, &plan)
            .await
            .map_err(|e| format!("failed to save cleanse plan after batch reconciliation: {e}"))?;

        if budget.exhausted() {
            return Ok(serde_json::json!({
                "ok": false,
                "kind": "batch_locked",
                "reason_code": controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted,
                "message": controller_kernel::batch_lock_error_message(controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted),
                "attempted_dataset_ids": attempted,
                "succeeded_dataset_ids": succeeded,
                "failed_dataset_ids": failed,
                "errors": [controller_kernel::batch_lock_error_message(controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted)],
            }));
        }

        let top_errors = res
            .get("errors")
            .cloned()
            .unwrap_or_else(|| serde_json::json!([]));
        Ok(serde_json::json!({
            "ok": ok && failed.is_empty(),
            "attempted_dataset_ids": attempted,
            "succeeded_dataset_ids": succeeded,
            "failed_dataset_ids": failed,
            "errors": top_errors,
            "inner": res,
        }))
    }
}

#[derive(Clone)]
pub struct ApplyNextModelBatchTool;

#[async_trait]
impl Tool for ApplyNextModelBatchTool {
    fn name(&self) -> &'static str {
        "apply_next_model_batch"
    }

    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
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
        let stg = dataset_truth::discover_staging_models_from_storage(ctx).await;
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
                "errors": [controller_kernel::batch_lock_error_message(controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted)],
                "attempted_item_names": [],
                "succeeded_item_names": [],
                "failed_item_names": [],
            }));
        }

        // Keep batch selection consistent with the suite driver via shared next-action resolution.
        let next_action = plan::model_next_authoring_action(&plan);
        let batch_names = next_action.author_sql_ids();
        if batch_names.is_empty() {
            if let plan::AuthoringNextAction::AuthorSchema(ids) = &next_action {
                return Ok(serde_json::json!({
                    "ok": true,
                    "kind": "defer_schema_batch",
                    "message": "next deterministic action is schema checklist work; call apply_next_model_schema_batch",
                    "pending_schema_contract_item_names": ids,
                    "attempted_item_names": [],
                    "succeeded_item_names": [],
                    "failed_item_names": [],
                    "errors": [],
                }));
            }
            if matches!(next_action, plan::AuthoringNextAction::Validate) {
                return Ok(serde_json::json!({
                    "ok": true,
                    "kind": "defer_validate",
                    "message": "next deterministic action is validate; transition to model_validate",
                    "attempted_item_names": [],
                    "succeeded_item_names": [],
                    "failed_item_names": [],
                    "errors": [],
                }));
            }
            let pending_schema = plan::model_pending_schema_contracts(&plan);
            if !pending_schema.is_empty() {
                return Ok(serde_json::json!({
                    "ok": true,
                    "message": "no remaining model SQL tasks; schema contracts pending",
                    "done": false,
                    "pending_schema_contract_item_names": pending_schema,
                    "attempted_item_names": [],
                    "succeeded_item_names": [],
                    "failed_item_names": [],
                }));
            }
            let done = plan::model_all_done(&plan);
            if done {}
            if !done {
                let mut blocked: Vec<String> = Vec::new();
                for t in plan.tasks.iter() {
                    let st = sql_model_checklist_status(&t.checklist);
                    if matches!(st, plan::ChecklistItemStatus::Blocked) {
                        blocked.push(t.name.clone());
                    }
                }
                blocked.sort();
                blocked.dedup();
                let msg = "no runnable model SQL tasks remain, but plan is not complete (blocked tasks exist)";
                return Ok(serde_json::json!({
                    "ok": false,
                    "kind": "plan_blocked",
                    "plan_key": plan.plan_key,
                    "message": msg,
                    "errors": [msg],
                    "blocked_item_names": blocked,
                    "attempted_item_names": [],
                    "succeeded_item_names": [],
                    "failed_item_names": [],
                }));
            }
            return Ok(serde_json::json!({
                "ok": true,
                "message": "no remaining model tasks in next batch (all done)",
                "done": done,
                "pending_schema_contract_item_names": pending_schema,
                "attempted_item_names": [],
                "succeeded_item_names": [],
                "failed_item_names": [],
            }));
        }
        if let Err(e) =
            chunk_progress_contract::enforce_chunk_contract(&batch_names, 5, "model_sql")
        {
            return Ok(serde_json::json!({
                "ok": false,
                "kind": "chunk_contract_violation",
                "errors": [e],
                "attempted_item_names": [],
                "succeeded_item_names": [],
                "failed_item_names": [],
            }));
        }

        // Truth gating: gold/model must only rely on existing silver models under models/staging/.
        let mut gating_errors: Vec<String> = Vec::new();
        for n in batch_names.iter() {
            if let Some(t) = plan.tasks.iter().find(|t| t.name == *n) {
                for inp in t.inputs.iter() {
                    let it = inp.trim();
                    if it.is_empty() {
                        continue;
                    }
                    if !dataset_truth::is_ref_only_gold_input(it) {
                        gating_errors.push(format!(
                            "{n}: invalid gold input '{it}' (gold must read from stg_* only)"
                        ));
                        continue;
                    }
                    if !stg.allowed_models.contains(it) {
                        gating_errors.push(format!(
                            "{n}: missing silver model input '{it}' (not present under models/staging/)"
                        ));
                    }
                }
            }
        }
        if !gating_errors.is_empty() {
            mark_needs_update_model(
                &mut plan,
                &batch_names,
                "gold inputs are not grounded in existing silver models under models/staging/",
            );
            controller_kernel::note_batch_result(&mut plan.progress, false);
            plan::save_model_plan(ctx, &plan)
                .await
                .map_err(|e| format!("failed to save model plan after input gating failure: {e}"))?;
            return Ok(serde_json::json!({
                "ok": false,
                "attempted_item_names": batch_names,
                "succeeded_item_names": [],
                "failed_item_names": batch_names,
                "errors": gating_errors,
            }));
        }

        let instructions = extract_string_arg(&args, "instructions")
            .or_else(|| extract_string_arg(&args, "user_instructions"));

        // Use explicit exec context (workgroup/checklist) when present so we can mark the correct
        // checklist item complete and avoid infinite loops on secondary SQL checklist items.
        let checklist_item_id = ctx
            .exec_ctx
            .as_ref()
            .and_then(|c| c.checklist_item_id.as_ref())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| plan::CHECKLIST_SQL_MODEL.to_string());

        if checklist_item_id == plan::CHECKLIST_SQL_MODEL {
            mark_in_progress_model(&mut plan, &batch_names);
        } else {
            mark_in_progress_model_checklist(&mut plan, &batch_names, &checklist_item_id);
        }
        plan::save_model_plan(ctx, &plan)
            .await
            .map_err(|e| format!("failed to save model plan after marking in-progress: {e}"))?;

        // Build gold_model items deterministically from the plan.
        let mut items: Vec<Value> = Vec::new();
        for n in batch_names.iter() {
            if let Some(t) = plan.tasks.iter().find(|t| t.name == *n) {
                let mut it = serde_json::json!({
                    "name": t.name,
                    "folder": t.folder,
                    "goal": t.goal,
                    "inputs": t.inputs,
                });
                if let Some(i) = instructions.as_ref() {
                    it["instructions"] = Value::String(i.clone());
                }
                items.push(it);
            } else {
                // Should not happen, but keep tool behavior predictable.
                items.push(serde_json::json!({"name": n, "inputs": []}));
            }
        }

        let inner = tools::gold_model::GoldModelTool;
        let res = match inner.call(serde_json::json!({ "items": items }), ctx).await {
            Ok(v) => v,
            Err(e) => {
                mark_needs_update_model(
                    &mut plan,
                    &batch_names,
                    &format!("apply_next_model_batch failed: {}", e.trim()),
                );
                controller_kernel::note_batch_result(&mut plan.progress, false);
                plan::save_model_plan(ctx, &plan)
                    .await
                    .map_err(|e| format!("failed to save model plan after batch tool failure: {e}"))?;
                return Ok(serde_json::json!({
                    "ok": false,
                    "attempted_item_names": batch_names,
                    "succeeded_item_names": [],
                    "failed_item_names": batch_names,
                    "errors": [e],
                }));
            }
        };

        let ok = res.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        let succeeded: Vec<String> = res
            .get("succeeded_item_names")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let succ_set: HashSet<String> = succeeded.iter().cloned().collect();
        let failed: Vec<String> = batch_names
            .iter()
            .filter(|n| !succ_set.contains(*n))
            .cloned()
            .collect();

        for n in succeeded.iter() {
            if checklist_item_id == plan::CHECKLIST_SQL_MODEL {
                plan::model_mark_done(&mut plan, n);
            } else {
                plan::model_checklist_mark_status(
                    &mut plan,
                    n,
                    &checklist_item_id,
                    plan::ChecklistItemStatus::Done,
                );
            }
        }
        if !failed.is_empty() || !ok {
            let err = res
                .get("errors")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .unwrap_or("batch failed");
            let _ = err;
            for n in failed.iter() {
                if checklist_item_id == plan::CHECKLIST_SQL_MODEL {
                    plan::model_mark_needs_update(&mut plan, n);
                } else {
                    plan::model_checklist_mark_status(
                        &mut plan,
                        n,
                        &checklist_item_id,
                        plan::ChecklistItemStatus::NeedsUpdate,
                    );
                }
            }
            let mut failed_targets: Vec<FailedModelRef> = Vec::new();
            for n in failed.iter() {
                let expected_path = plan
                    .tasks
                    .iter()
                    .find(|t| t.name == *n)
                    .and_then(|t| t.expected_model_path.clone());
                failed_targets.push(FailedModelRef {
                    name: n.clone(),
                    file: expected_path.unwrap_or_default(),
                });
            }
            let errors: Vec<String> = res
                .get("errors")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let kind = classify_batch_failure_kind(&errors);
            let brief = if err.trim().is_empty() {
                "apply_next_model_batch failed".to_string()
            } else {
                err.to_string()
            };
            emit_batch_event(
                ctx,
                DataEngineerEvent::BatchAuthoringFailed {
                    tier: ExecutionTier::Model,
                    kind,
                    failed_targets,
                    brief,
                },
            )
            .await?;
        } else {
            emit_batch_event(
                ctx,
                DataEngineerEvent::BatchAuthoringRecovered,
            )
            .await?;
        }

        let budget = controller_kernel::note_batch_result(&mut plan.progress, ok && failed.is_empty());
        plan::save_model_plan(ctx, &plan)
            .await
            .map_err(|e| format!("failed to save model plan after batch reconciliation: {e}"))?;

        if budget.exhausted() {
            return Ok(serde_json::json!({
                "ok": false,
                "kind": "batch_locked",
                "reason_code": controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted,
                "message": controller_kernel::batch_lock_error_message(controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted),
                "attempted_item_names": batch_names,
                "succeeded_item_names": succeeded,
                "failed_item_names": failed,
                "errors": [controller_kernel::batch_lock_error_message(controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted)],
            }));
        }

        let top_errors = res
            .get("errors")
            .cloned()
            .unwrap_or_else(|| serde_json::json!([]));
        Ok(serde_json::json!({
            "ok": ok && failed.is_empty(),
            "attempted_item_names": batch_names,
            "succeeded_item_names": succeeded,
            "failed_item_names": failed,
            "errors": top_errors,
            "inner": res,
        }))
    }
}
