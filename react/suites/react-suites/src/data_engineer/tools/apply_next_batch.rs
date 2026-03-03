use async_trait::async_trait;
use serde_json::Value;
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
        .find(|it| it.checklist_item_id == plan::CHECKLIST_SQL_MODEL)
        .map(|it| it.status)
        .unwrap_or(plan::ChecklistItemStatus::Pending)
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
                let kind = crate::data_engineer::tools::batch_sql_runner::classify_batch_failure_kind(&[e.clone()]);
                controller_kernel::note_batch_result_with_failure_kind(
                    &mut plan.progress,
                    false,
                    Some(kind),
                );
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
        let succeeded =
            crate::data_engineer::tools::batch_sql_runner::parse_succeeded_ids(
                &res,
                "succeeded_dataset_ids",
            );
        let attempted: Vec<String> = batch.clone();

        // Compute failed as attempted - succeeded.
        let failed = crate::data_engineer::tools::batch_sql_runner::derive_failed_ids(
            &attempted,
            &succeeded,
        );

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
        let mut failure_kind_for_budget: Option<BatchFailureKind> = None;
        if !failed.is_empty() || !ok {
            let err = crate::data_engineer::tools::batch_sql_runner::extract_first_error(
                &res,
                "batch failed",
            );
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
            let errors = crate::data_engineer::tools::batch_sql_runner::extract_errors(&res);
            let kind = crate::data_engineer::tools::batch_sql_runner::classify_batch_failure_kind(&errors);
            failure_kind_for_budget = Some(kind);
            let brief = if err.trim().is_empty() {
                "apply_next_cleanse_batch failed".to_string()
            } else {
                err
            };
            crate::data_engineer::tools::batch_sql_runner::emit_batch_event(
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
            crate::data_engineer::tools::batch_sql_runner::emit_batch_event(
                ctx,
                DataEngineerEvent::BatchAuthoringRecovered,
            )
            .await?;
        }
        let budget = controller_kernel::note_batch_result_with_failure_kind(
            &mut plan.progress,
            ok && failed.is_empty(),
            failure_kind_for_budget,
        );
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
                let kind = crate::data_engineer::tools::batch_sql_runner::classify_batch_failure_kind(&[e.clone()]);
                controller_kernel::note_batch_result_with_failure_kind(
                    &mut plan.progress,
                    false,
                    Some(kind),
                );
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
        let succeeded =
            crate::data_engineer::tools::batch_sql_runner::parse_succeeded_ids(
                &res,
                "succeeded_item_names",
            );
        let failed = crate::data_engineer::tools::batch_sql_runner::derive_failed_ids(
            &batch_names,
            &succeeded,
        );

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
        let mut failure_kind_for_budget: Option<BatchFailureKind> = None;
        if !failed.is_empty() || !ok {
            let err = crate::data_engineer::tools::batch_sql_runner::extract_first_error(
                &res,
                "batch failed",
            );
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
            let errors = crate::data_engineer::tools::batch_sql_runner::extract_errors(&res);
            let kind = crate::data_engineer::tools::batch_sql_runner::classify_batch_failure_kind(&errors);
            failure_kind_for_budget = Some(kind);
            let brief = if err.trim().is_empty() {
                "apply_next_model_batch failed".to_string()
            } else {
                err
            };
            crate::data_engineer::tools::batch_sql_runner::emit_batch_event(
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
            crate::data_engineer::tools::batch_sql_runner::emit_batch_event(
                ctx,
                DataEngineerEvent::BatchAuthoringRecovered,
            )
            .await?;
        }

        let budget = controller_kernel::note_batch_result_with_failure_kind(
            &mut plan.progress,
            ok && failed.is_empty(),
            failure_kind_for_budget,
        );
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

#[cfg(test)]
mod tests {
    use super::*;
    use react_core::keyspace::{DefaultKeyspace, Keyspace};
    use react_core::llm::{ChatMessage, LargeLanguageModel};
    use react_core::scope::RequestScope;
    use react_core::storage::{InMemoryStorageAdapter, StorageAdapter};
    use std::sync::{Arc, Mutex};

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

    fn minimal_cfg() -> Arc<crate::config::ReactResolvedConfig> {
        Arc::new(crate::config::ReactResolvedConfig {
            server: crate::config::ServerResolved { port: 1 },
            storage: crate::config::StorageResolved {
                mode: react_core::resolved_config::StorageMode::Local,
                bucket: None,
                path: None,
            },
            scope: RequestScope {
                tenant: "t".to_string(),
                workspace: "w".to_string(),
                project_id: "p".to_string(),
            },
            llm: crate::config::LlmResolved::default(),
            providers: crate::config::ProvidersResolved {
                warehouse: crate::config::WarehouseResolved {
                    kind: react_core::resolved_config::WarehouseKind::Athena,
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

    fn test_ctx(thread_id: &str) -> AgentCtx {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        let llm: Arc<dyn LargeLanguageModel> = Arc::new(ScriptedLlm {
            replies: Mutex::new(vec![]),
        });
        AgentCtx {
            top_k: 1,
            per_step_timeout_secs: 1,
            max_steps: 2,
            thread_id: Some(thread_id.to_string()),
            progress_tx: None,
            pre_step_tx: None,
            trace_tx: None,
            agent_name: Some("test".to_string()),
            policy: Arc::new(react_core::agent::DefaultPolicy),
            llm,
            storage,
            scope: RequestScope {
                tenant: "t".to_string(),
                workspace: "w".to_string(),
                project_id: "p".to_string(),
            },
            keyspace,
            query: None,
            warehouse: Arc::new(react_core::providers::NullWarehouseProvider::default()),
            dbt: None,
            vector: None,
            thread_store: None,
            exec_ctx: None,
            resolved_config: Some(minimal_cfg()),
        }
    }

    async fn seed_cleanse_plan(ctx: &AgentCtx, sql_done: bool, schema_done: bool, locked: bool) {
        let mut checklist = plan::canonical_task_checklist(true);
        if let Some(item) = checklist
            .iter_mut()
            .find(|it| it.checklist_item_id == plan::CHECKLIST_SQL_MODEL)
        {
            item.status = if sql_done {
                plan::ChecklistItemStatus::Done
            } else {
                plan::ChecklistItemStatus::Pending
            };
        }
        if let Some(item) = checklist
            .iter_mut()
            .find(|it| it.checklist_item_id == plan::CHECKLIST_SCHEMA_CONTRACT)
        {
            item.status = if schema_done {
                plan::ChecklistItemStatus::Done
            } else {
                plan::ChecklistItemStatus::Pending
            };
        }
        let batches = vec![vec!["AwsDataCatalog.test_raw.raw_customers".to_string()]];
        let mut progress = plan::PlanProgress::default();
        if locked {
            progress.consecutive_batch_failures = controller_kernel::max_consecutive_batch_failures();
        }
        let plan_doc = plan::CleansePlan {
            plan_key: plan::new_cleanse_plan_key(ctx),
            status: plan::PlanStatus::Approved,
            project_snapshot: serde_json::json!({}),
            tasks: vec![plan::CleanseTask {
                dataset_id: "AwsDataCatalog.test_raw.raw_customers".to_string(),
                expected_model_path: Some("models/staging/stg_test_raw_raw_customers.sql".to_string()),
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
        plan::save_cleanse_plan(ctx, &plan_doc).await.unwrap();
    }

    async fn seed_model_plan(ctx: &AgentCtx, sql_done: bool, schema_done: bool, locked: bool) {
        let stg_rel = "models/staging/stg_test_raw_raw_customers.sql";
        let stg_key = crate::data_engineer::files_store::join_storage_key(ctx, stg_rel);
        ctx.storage
            .put_bytes(
                &stg_key,
                b"select 1 as customer_id, 'x' as email",
                "text/sql",
            )
            .await
            .unwrap();

        let mut checklist = plan::canonical_task_checklist(false);
        if let Some(item) = checklist
            .iter_mut()
            .find(|it| it.checklist_item_id == plan::CHECKLIST_SQL_MODEL)
        {
            item.status = if sql_done {
                plan::ChecklistItemStatus::Done
            } else {
                plan::ChecklistItemStatus::Pending
            };
        }
        if let Some(item) = checklist
            .iter_mut()
            .find(|it| it.checklist_item_id == plan::CHECKLIST_SCHEMA_CONTRACT)
        {
            item.status = if schema_done {
                plan::ChecklistItemStatus::Done
            } else {
                plan::ChecklistItemStatus::Pending
            };
        }
        let batches = vec![vec!["dim_customers".to_string()]];
        let mut progress = plan::PlanProgress::default();
        if locked {
            progress.consecutive_batch_failures = controller_kernel::max_consecutive_batch_failures();
        }
        let plan_doc = plan::ModelPlan {
            plan_key: plan::new_model_plan_key(ctx),
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
        plan::save_model_plan(ctx, &plan_doc).await.unwrap();
    }

    #[tokio::test]
    async fn apply_next_cleanse_batch_contract_defer_schema() {
        let ctx = test_ctx("tid-cleanse-defer-schema");
        seed_cleanse_plan(&ctx, true, false, false).await;
        let res = ApplyNextCleanseBatchTool { datasets: None }
            .call(serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert_eq!(res.get("kind").and_then(|v| v.as_str()), Some("defer_schema_batch"));
        assert_eq!(
            res.get("attempted_dataset_ids").and_then(|v| v.as_array()).map(|a| a.len()),
            Some(0)
        );
        assert_eq!(
            res.get("succeeded_dataset_ids").and_then(|v| v.as_array()).map(|a| a.len()),
            Some(0)
        );
        assert_eq!(
            res.get("failed_dataset_ids").and_then(|v| v.as_array()).map(|a| a.len()),
            Some(0)
        );
    }

    #[tokio::test]
    async fn apply_next_cleanse_batch_contract_defer_validate() {
        let ctx = test_ctx("tid-cleanse-defer-validate");
        seed_cleanse_plan(&ctx, true, true, false).await;
        let res = ApplyNextCleanseBatchTool { datasets: None }
            .call(serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert_eq!(res.get("kind").and_then(|v| v.as_str()), Some("defer_validate"));
        assert_eq!(
            res.get("attempted_dataset_ids").and_then(|v| v.as_array()).map(|a| a.len()),
            Some(0)
        );
    }

    #[tokio::test]
    async fn apply_next_cleanse_batch_contract_batch_locked() {
        let ctx = test_ctx("tid-cleanse-locked");
        seed_cleanse_plan(&ctx, false, false, true).await;
        let res = ApplyNextCleanseBatchTool { datasets: None }
            .call(serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert_eq!(res.get("kind").and_then(|v| v.as_str()), Some("batch_locked"));
        assert_eq!(
            res.get("attempted_dataset_ids").and_then(|v| v.as_array()).map(|a| a.len()),
            Some(0)
        );
    }

    #[tokio::test]
    async fn apply_next_model_batch_contract_defer_schema() {
        let ctx = test_ctx("tid-model-defer-schema");
        seed_model_plan(&ctx, true, false, false).await;
        let res = ApplyNextModelBatchTool
            .call(serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert_eq!(res.get("kind").and_then(|v| v.as_str()), Some("defer_schema_batch"));
        assert_eq!(
            res.get("attempted_item_names").and_then(|v| v.as_array()).map(|a| a.len()),
            Some(0)
        );
    }

    #[tokio::test]
    async fn apply_next_model_batch_contract_defer_validate() {
        let ctx = test_ctx("tid-model-defer-validate");
        seed_model_plan(&ctx, true, true, false).await;
        let res = ApplyNextModelBatchTool
            .call(serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert_eq!(res.get("kind").and_then(|v| v.as_str()), Some("defer_validate"));
        assert_eq!(
            res.get("attempted_item_names").and_then(|v| v.as_array()).map(|a| a.len()),
            Some(0)
        );
    }

    #[tokio::test]
    async fn apply_next_model_batch_contract_batch_locked() {
        let ctx = test_ctx("tid-model-locked");
        seed_model_plan(&ctx, false, false, true).await;
        let res = ApplyNextModelBatchTool
            .call(serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert_eq!(res.get("kind").and_then(|v| v.as_str()), Some("batch_locked"));
        assert_eq!(
            res.get("attempted_item_names").and_then(|v| v.as_array()).map(|a| a.len()),
            Some(0)
        );
    }
}
