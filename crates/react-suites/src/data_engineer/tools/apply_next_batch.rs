use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;

use react_core::agent::AgentCtx;
use react_core::providers::DatasetCatalogProvider;
use react_core::tools::Tool;

use crate::data_engineer::plan;
use crate::data_engineer::plan::{CleansePlan, ModelPlan, TaskStatus};
use crate::data_engineer::tools;

const MAX_CONSECUTIVE_BATCH_FAILURES: usize = 3;

fn extract_string_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|x| x.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn mark_in_progress_cleanse(plan: &mut CleansePlan, dataset_ids: &[String]) {
    for ds in dataset_ids.iter() {
        if let Some(t) = plan.tasks.iter_mut().find(|t| t.dataset_id == *ds) {
            t.status = TaskStatus::InProgress;
        }
    }
}

fn mark_in_progress_model(plan: &mut ModelPlan, names: &[String]) {
    for n in names.iter() {
        if let Some(t) = plan.tasks.iter_mut().find(|t| t.name == *n) {
            t.status = TaskStatus::InProgress;
        }
    }
}

fn mark_needs_update_cleanse(plan: &mut CleansePlan, dataset_ids: &[String], note: &str) {
    for ds in dataset_ids.iter() {
        if let Some(t) = plan.tasks.iter_mut().find(|t| t.dataset_id == *ds) {
            t.status = TaskStatus::NeedsUpdate;
            if !note.trim().is_empty() && !t.notes.iter().any(|n| n.trim() == note.trim()) {
                t.notes.push(note.trim().to_string());
            }
        }
    }
}

fn mark_needs_update_model(plan: &mut ModelPlan, names: &[String], note: &str) {
    for n in names.iter() {
        if let Some(t) = plan.tasks.iter_mut().find(|t| t.name == *n) {
            t.status = TaskStatus::NeedsUpdate;
            if !note.trim().is_empty() && !t.notes.iter().any(|x| x.trim() == note.trim()) {
                t.notes.push(note.trim().to_string());
            }
        }
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

#[derive(Clone)]
pub struct ApplyNextCleanseBatchTool {
    pub datasets: Option<Arc<dyn DatasetCatalogProvider>>,
}

#[async_trait]
impl Tool for ApplyNextCleanseBatchTool {
    fn name(&self) -> &'static str { "apply_next_cleanse_batch" }

    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
        let mut plan = plan::load_cleanse_plan(ctx)
            .await
            .ok_or_else(|| "no active cleanse plan found".to_string())?;
        if plan.status != plan::PlanStatus::Approved && plan.status != plan::PlanStatus::Completed {
            return Err(format!(
                "cleanse plan is not approved (status={:?}); return to plan phase",
                plan.status
            ));
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

        let batch = plan::cleanse_next_batch(&plan);
        if batch.is_empty() {
            return Ok(serde_json::json!({
                "ok": true,
                "message": "no remaining cleanse tasks in next batch (all done)",
                "attempted_dataset_ids": [],
                "succeeded_dataset_ids": [],
                "failed_dataset_ids": [],
            }));
        }

        // Optional user instruction passthrough to inner tool.
        let instructions = extract_string_arg(&args, "instructions")
            .or_else(|| extract_string_arg(&args, "user_instructions"));

        // Mark batch in progress before running (so plan snapshot reflects active remediation).
        mark_in_progress_cleanse(&mut plan, &batch);
        let _ = plan::save_cleanse_plan(ctx, &plan).await;

        let mut inner_args = serde_json::json!({ "dataset_ids": batch.clone() });
        if let Some(i) = instructions {
            inner_args["instructions"] = Value::String(i);
        }

        let inner = tools::staging_model::StagingModelTool { datasets: self.datasets.clone() };
        let res = match inner.call(inner_args, ctx).await {
            Ok(v) => v,
            Err(e) => {
                // Tool error: mark whole batch as needs_update and return a structured failure (not an Err),
                // so callers can render it and retry deterministically.
                mark_needs_update_cleanse(&mut plan, &batch, &format!("apply_next_cleanse_batch failed: {}", e.trim()));
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
            .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
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
            plan::cleanse_mark_done(&mut plan, ds);
        }
        if !failed.is_empty() || !ok {
            let err = res
                .get("errors")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .unwrap_or("batch failed");
            let note = format!("apply_next_cleanse_batch failed: {}", err.trim());
            mark_needs_update_cleanse(&mut plan, &failed, &note);
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

        let top_errors = res.get("errors").cloned().unwrap_or_else(|| serde_json::json!([]));
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
    fn name(&self) -> &'static str { "apply_next_model_batch" }

    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
        let mut plan = plan::load_model_plan(ctx)
            .await
            .ok_or_else(|| "no active model plan found".to_string())?;
        if plan.status != plan::PlanStatus::Approved && plan.status != plan::PlanStatus::Completed {
            return Err(format!(
                "model plan is not approved (status={:?}); return to plan phase",
                plan.status
            ));
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

        let batch_names = plan::model_next_batch(&plan);
        if batch_names.is_empty() {
            return Ok(serde_json::json!({
                "ok": true,
                "message": "no remaining model tasks in next batch (all done)",
                "attempted_item_names": [],
                "succeeded_item_names": [],
                "failed_item_names": [],
            }));
        }

        let instructions = extract_string_arg(&args, "instructions")
            .or_else(|| extract_string_arg(&args, "user_instructions"));

        mark_in_progress_model(&mut plan, &batch_names);
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
                mark_needs_update_model(&mut plan, &batch_names, &format!("apply_next_model_batch failed: {}", e.trim()));
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
            .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default();
        let succ_set: HashSet<String> = succeeded.iter().cloned().collect();
        let failed: Vec<String> = batch_names
            .iter()
            .filter(|n| !succ_set.contains(*n))
            .cloned()
            .collect();

        for n in succeeded.iter() {
            plan::model_mark_done(&mut plan, n);
        }
        if !failed.is_empty() || !ok {
            let err = res
                .get("errors")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .unwrap_or("batch failed");
            mark_needs_update_model(&mut plan, &failed, &format!("apply_next_model_batch failed: {}", err));
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

        let top_errors = res.get("errors").cloned().unwrap_or_else(|| serde_json::json!([]));
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

