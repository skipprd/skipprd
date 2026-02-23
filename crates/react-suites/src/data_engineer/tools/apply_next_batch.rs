use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;

use react_core::agent::AgentCtx;
use react_core::control_flow::PhaseReasonCode;
use react_core::providers::DatasetCatalogProvider;
use react_core::tools::Tool;

use crate::data_engineer::dataset_truth;
use crate::data_engineer::control_flow;
use crate::data_engineer::plan;
use crate::data_engineer::plan::{CleansePlan, ModelPlan};
use crate::data_engineer::tools;

pub(crate) const MAX_CONSECUTIVE_BATCH_FAILURES: usize = 3;

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

fn mark_in_progress_cleanse_checklist(plan: &mut CleansePlan, dataset_ids: &[String], checklist: &str) {
    for ds in dataset_ids.iter() {
        plan::cleanse_checklist_mark_status(plan, ds, checklist, plan::ChecklistItemStatus::InProgress);
    }
}

fn mark_in_progress_model(plan: &mut ModelPlan, names: &[String]) {
    for n in names.iter() {
        plan::model_mark_in_progress(plan, n);
    }
}

fn mark_in_progress_model_checklist(plan: &mut ModelPlan, names: &[String], checklist: &str) {
    for n in names.iter() {
        plan::model_checklist_mark_status(plan, n, checklist, plan::ChecklistItemStatus::InProgress);
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

fn update_failure_counters(progress: &mut plan::PlanProgress, ok: bool) {
    if ok {
        progress.consecutive_batch_failures = 0;
        return;
    }
    progress.consecutive_batch_failures = progress.consecutive_batch_failures.saturating_add(1);
    progress.total_batch_failures = progress.total_batch_failures.saturating_add(1);
}

fn sql_model_checklist_status(items: &[plan::PlanChecklistItem]) -> plan::ChecklistItemStatus {
    items
        .iter()
        .find(|it| it.checklist_item_id == "sql_model")
        .map(|it| it.status)
        .unwrap_or(plan::ChecklistItemStatus::Pending)
}

async fn maybe_advance_phase_on_done(
    ctx: &AgentCtx,
    from_phase: control_flow::Phase,
    to_phase: control_flow::Phase,
    reason_code: PhaseReasonCode,
    detail: Value,
) {
    let (Some(store), Some(tid)) = (ctx.thread_store.as_ref(), ctx.thread_id.as_deref()) else {
        return;
    };
    let log = store.get(tid).await.ok();
    let cur = control_flow::phase_from_log(log.as_ref());
    if cur != from_phase {
        return;
    }
    control_flow::append_phase_with_reason(
        store,
        tid,
        Some("agent".to_string()),
        Some(cur),
        to_phase,
        Some(reason_code),
        Some(detail),
    )
    .await
    .unwrap_or_else(|e| {
        tracing::warn!("failed to append phase transition: {}", e);
    });
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

        if plan.progress.consecutive_batch_failures >= MAX_CONSECUTIVE_BATCH_FAILURES {
            return Ok(serde_json::json!({
                "ok": false,
                "errors": ["too many consecutive batch failures; apply a targeted fix (dbt_files op=patch) or ask the user for guidance before retrying"],
                "attempted_dataset_ids": [],
                "succeeded_dataset_ids": [],
                "failed_dataset_ids": [],
            }));
        }

        // Keep batch selection consistent with the suite driver: work-group driven only.
        let next_action = plan::cleanse_next_action(&plan);
        let batch = match next_action.as_ref() {
            Some((plan::WorkGroupKind::AuthorSql, ds)) => ds.clone(),
            _ => Vec::new(),
        };
        if batch.is_empty() {
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
            if done {
                maybe_advance_phase_on_done(
                    ctx,
                    control_flow::Phase::CleanseAuthor,
                    control_flow::Phase::CleanseValidate,
                    PhaseReasonCode::NoWorkAllDone,
                    serde_json::json!({ "plan_key": plan.plan_key }),
                )
                .await;
            }
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
                return Ok(serde_json::json!({
                    "ok": false,
                    "kind": "plan_blocked",
                    "plan_key": plan.plan_key,
                    "message": "no runnable cleanse SQL tasks remain, but plan is not complete (blocked tasks exist)",
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
                update_failure_counters(&mut plan.progress, false);
                let _ = plan::save_cleanse_plan(ctx, &plan).await;
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
        let _ = plan::save_cleanse_plan(ctx, &plan).await;

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
                update_failure_counters(&mut plan.progress, false);
                let _ = plan::save_cleanse_plan(ctx, &plan).await;
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
        }
        update_failure_counters(&mut plan.progress, ok && failed.is_empty());
        let _ = plan::save_cleanse_plan(ctx, &plan).await;

        if plan.progress.consecutive_batch_failures >= MAX_CONSECUTIVE_BATCH_FAILURES {
            return Ok(serde_json::json!({
                "ok": false,
                "attempted_dataset_ids": attempted,
                "succeeded_dataset_ids": succeeded,
                "failed_dataset_ids": failed,
                "errors": ["too many consecutive batch failures; ask user for guidance or apply targeted dbt_files patches before retrying"],
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
        let v = plan::ensure_model_plan_semantically_valid_or_repaired(ctx, &mut plan, &stg.allowed_models).await?;
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

        if plan.progress.consecutive_batch_failures >= MAX_CONSECUTIVE_BATCH_FAILURES {
            return Ok(serde_json::json!({
                "ok": false,
                "errors": ["too many consecutive batch failures; apply a targeted fix (dbt_files op=patch) or ask the user for guidance before retrying"],
                "attempted_item_names": [],
                "succeeded_item_names": [],
                "failed_item_names": [],
            }));
        }

        // Keep batch selection consistent with the suite driver: work-group driven only.
        let next_action = plan::model_next_action(&plan);
        let batch_names = match next_action.as_ref() {
            Some((plan::WorkGroupKind::AuthorSql, names)) => names.clone(),
            _ => Vec::new(),
        };
        if batch_names.is_empty() {
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
            if done {
                maybe_advance_phase_on_done(
                    ctx,
                    control_flow::Phase::ModelAuthor,
                    control_flow::Phase::ModelValidate,
                    PhaseReasonCode::NoWorkAllDone,
                    serde_json::json!({ "plan_key": plan.plan_key }),
                )
                .await;
            }
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
                return Ok(serde_json::json!({
                    "ok": false,
                    "kind": "plan_blocked",
                    "plan_key": plan.plan_key,
                    "message": "no runnable model SQL tasks remain, but plan is not complete (blocked tasks exist)",
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
            update_failure_counters(&mut plan.progress, false);
            let _ = plan::save_model_plan(ctx, &plan).await;
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
        let _ = plan::save_model_plan(ctx, &plan).await;

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
                update_failure_counters(&mut plan.progress, false);
                let _ = plan::save_model_plan(ctx, &plan).await;
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
        }

        update_failure_counters(&mut plan.progress, ok && failed.is_empty());
        let _ = plan::save_model_plan(ctx, &plan).await;

        if plan.progress.consecutive_batch_failures >= MAX_CONSECUTIVE_BATCH_FAILURES {
            return Ok(serde_json::json!({
                "ok": false,
                "attempted_item_names": batch_names,
                "succeeded_item_names": succeeded,
                "failed_item_names": failed,
                "errors": ["too many consecutive batch failures; ask user for guidance or apply targeted dbt_files patches before retrying"],
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
