use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

use crate::providers::DatasetCatalogProvider;
use react_core::agent::AgentCtx;
use react_core::tools::Tool;

use crate::chunk_progress_contract;
use crate::controller_kernel;
use crate::dataset_truth;
use crate::failure_kind::FailureKind;
use crate::plan;
use crate::plan::{CleansePlan, ModelPlan};
use crate::progress_controller::{DataEngineerEvent, ExecutionTier};
use crate::tools;

use super::model_authoring_engine::extract_string_arg;

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

async fn emit_batch_failure(ctx: &AgentCtx, tier: ExecutionTier, err: &str) -> Result<(), String> {
    let brief = if err.trim().is_empty() {
        format!("batch authoring failed (tier={tier:?})")
    } else {
        err.to_string()
    };
    crate::tools::batch_sql_runner::emit_batch_event(
        ctx,
        DataEngineerEvent::EvaluationVerdictRecorded {
            verdict: crate::evaluation::EvaluationVerdictSummary {
                kind: crate::evaluation::VerdictKind::RepairImplementation,
                message: brief.clone(),
            },
            evidence_hash: react_core::llm_observability::sha256_hex_str(&brief),
        },
    )
    .await
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
    for ds in dataset_ids.iter() {
        plan::cleanse_mark_needs_update(plan, ds, Some(note));
    }
}

fn mark_needs_update_model(plan: &mut ModelPlan, names: &[String], note: &str) {
    for n in names.iter() {
        plan::model_mark_needs_update(plan, n, Some(note));
    }
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

        let next_action = plan::cleanse_next_authoring_action(&plan);
        let batch = next_action.author_sql_ids();
        if batch.is_empty() {
            return Ok(serde_json::json!({
                "ok": true,
                "progress_made": false,
                "message": "no cleanse SQL work remains; all pending datasets have been processed",
                "attempted_dataset_ids": [],
                "succeeded_dataset_ids": [],
                "failed_dataset_ids": [],
            }));
        }
        if let Err(e) = chunk_progress_contract::enforce_chunk_contract(
            &batch,
            crate::plan_progress::MAX_BATCH_SIZE,
            "cleanse_sql",
        ) {
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
        if let Some(q) = crate::ctx_ext::actx_query(ctx) {
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
                plan::save_cleanse_plan(ctx, &plan).await.map_err(|e| {
                    format!("failed to save cleanse plan after schema gating failure: {e}")
                })?;
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
            .exec_ctx()
            .as_ref()
            .and_then(|c| c.get_str("checklist_item_id"))
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

        tracing::info!(
            plan_key = %plan.plan_key,
            checklist_item_id = %checklist_item_id,
            datasets = ?batch,
            "apply_next_cleanse_batch: invoking staging_model to author cleanse SQL"
        );

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
                let err_brief = format!("apply_next_cleanse_batch failed: {}", e.trim());
                mark_needs_update_cleanse(&mut plan, &batch, &err_brief);
                controller_kernel::note_batch_result_with_failure_kind(
                    &mut plan.progress,
                    false,
                    Some(FailureKind::Unknown),
                );
                emit_batch_failure(ctx, ExecutionTier::Cleanse, &err_brief).await?;
                plan::save_cleanse_plan(ctx, &plan).await.map_err(|e| {
                    format!("failed to save cleanse plan after batch tool failure: {e}")
                })?;
                return Ok(serde_json::json!({
                    "ok": false,
                    "attempted_dataset_ids": batch,
                    "succeeded_dataset_ids": [],
                    "failed_dataset_ids": batch,
                    "errors": [e],
                }));
            }
        };

        let ok = res.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        let succeeded =
            crate::tools::batch_sql_runner::parse_succeeded_ids(&res, "succeeded_dataset_ids");
        let attempted: Vec<String> = batch.clone();

        // Compute failed as attempted - succeeded.
        let failed = crate::tools::batch_sql_runner::derive_failed_ids(&attempted, &succeeded);

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
        let mut failure_kind_for_budget: Option<FailureKind> = None;
        if !failed.is_empty() || !ok {
            let err = crate::tools::batch_sql_runner::extract_first_error(&res, "batch failed");
            let note = format!("apply_next_cleanse_batch failed: {}", err.trim());
            let _ = note;
            for ds in failed.iter() {
                if checklist_item_id == plan::CHECKLIST_SQL_MODEL {
                    plan::cleanse_mark_needs_update(&mut plan, ds, Some(err.as_str()));
                } else {
                    plan::cleanse_checklist_mark_status(
                        &mut plan,
                        ds,
                        &checklist_item_id,
                        plan::ChecklistItemStatus::NeedsUpdate,
                    );
                }
            }
            let kind = crate::tools::batch_sql_runner::extract_batch_failure_kind(&res)
                .map_err(|e| format!("apply_next_cleanse_batch_contract_error: {e}"))?;
            failure_kind_for_budget = Some(kind);
            let brief = if err.trim().is_empty() {
                "apply_next_cleanse_batch failed".to_string()
            } else {
                err
            };
            crate::tools::batch_sql_runner::emit_batch_event(
                ctx,
                DataEngineerEvent::EvaluationVerdictRecorded {
                    verdict: crate::evaluation::EvaluationVerdictSummary {
                        kind: crate::evaluation::VerdictKind::RepairImplementation,
                        message: brief.clone(),
                    },
                    evidence_hash: react_core::llm_observability::sha256_hex_str(&brief),
                },
            )
            .await?;
        } else {
            crate::tools::batch_sql_runner::emit_batch_event(
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

        let stg = dataset_truth::discover_staging_models_from_storage(ctx).await;
        let v = crate::plan_semantic_gate::gate_model_plan(&mut plan, &stg.allowed_models)
            .into_validation();
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
        plan::reconcile_model_batches_and_work_groups(&mut plan);

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

        let next_action = plan::model_next_authoring_action(&plan);
        let batch_names = next_action.author_sql_ids();
        if batch_names.is_empty() {
            return Ok(serde_json::json!({
                "ok": true,
                "progress_made": false,
                "message": "no model SQL work remains; all pending items have been processed",
                "attempted_item_names": [],
                "succeeded_item_names": [],
                "failed_item_names": [],
            }));
        }
        if let Err(e) = chunk_progress_contract::enforce_chunk_contract(
            &batch_names,
            crate::plan_progress::MAX_BATCH_SIZE,
            "model_sql",
        ) {
            return Ok(serde_json::json!({
                "ok": false,
                "kind": "chunk_contract_violation",
                "errors": [e],
                "attempted_item_names": [],
                "succeeded_item_names": [],
                "failed_item_names": [],
            }));
        }

        // Truth gating: gold/model inputs must be either existing staging models
        // or other tasks within the same plan (intra-plan gold dependencies).
        let plan_task_names: std::collections::BTreeSet<String> = plan
            .tasks
            .iter()
            .map(|t| t.name.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let mut gating_errors: Vec<String> = Vec::new();
        for n in batch_names.iter() {
            if let Some(t) = plan.tasks.iter().find(|t| t.name == *n) {
                for inp in t.inputs.iter() {
                    let it = inp.trim();
                    if it.is_empty() {
                        continue;
                    }
                    if !dataset_truth::is_valid_gold_input(it, &plan_task_names) {
                        gating_errors.push(format!(
                            "{n}: invalid gold input '{it}' (must be a staging model or an intra-plan gold model)"
                        ));
                        continue;
                    }
                    if dataset_truth::is_staging_model_name(it) && !stg.allowed_models.contains(it)
                    {
                        gating_errors.push(format!(
                            "{n}: missing staging model input '{it}' (not present under models/staging/)"
                        ));
                    }
                }
            }
        }
        if !gating_errors.is_empty() {
            let err_brief = "gold inputs are not grounded in known staging models or plan tasks";
            mark_needs_update_model(&mut plan, &batch_names, err_brief);
            controller_kernel::note_batch_result(&mut plan.progress, false);
            emit_batch_failure(ctx, ExecutionTier::Model, err_brief).await?;
            plan::save_model_plan(ctx, &plan).await.map_err(|e| {
                format!("failed to save model plan after input gating failure: {e}")
            })?;
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
            .exec_ctx()
            .as_ref()
            .and_then(|c| c.get_str("checklist_item_id"))
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
                    "grounded_inputs": t.grounded_inputs,
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
                let err_brief = format!("apply_next_model_batch failed: {}", e.trim());
                mark_needs_update_model(&mut plan, &batch_names, &err_brief);
                controller_kernel::note_batch_result_with_failure_kind(
                    &mut plan.progress,
                    false,
                    Some(FailureKind::Unknown),
                );
                emit_batch_failure(ctx, ExecutionTier::Model, &err_brief).await?;
                plan::save_model_plan(ctx, &plan).await.map_err(|e| {
                    format!("failed to save model plan after batch tool failure: {e}")
                })?;
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
            crate::tools::batch_sql_runner::parse_succeeded_ids(&res, "succeeded_item_names");
        let failed = crate::tools::batch_sql_runner::derive_failed_ids(&batch_names, &succeeded);

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
        let mut failure_kind_for_budget: Option<FailureKind> = None;
        if !failed.is_empty() || !ok {
            let err = crate::tools::batch_sql_runner::extract_first_error(&res, "batch failed");
            for n in failed.iter() {
                if checklist_item_id == plan::CHECKLIST_SQL_MODEL {
                    plan::model_mark_needs_update(&mut plan, n, Some(err.as_str()));
                } else {
                    plan::model_checklist_mark_status(
                        &mut plan,
                        n,
                        &checklist_item_id,
                        plan::ChecklistItemStatus::NeedsUpdate,
                    );
                }
            }
            let kind = crate::tools::batch_sql_runner::extract_batch_failure_kind(&res)
                .map_err(|e| format!("apply_next_model_batch_contract_error: {e}"))?;
            failure_kind_for_budget = Some(kind);
            let brief = if err.trim().is_empty() {
                "apply_next_model_batch failed".to_string()
            } else {
                err
            };
            crate::tools::batch_sql_runner::emit_batch_event(
                ctx,
                DataEngineerEvent::EvaluationVerdictRecorded {
                    verdict: crate::evaluation::EvaluationVerdictSummary {
                        kind: crate::evaluation::VerdictKind::RepairImplementation,
                        message: brief.clone(),
                    },
                    evidence_hash: react_core::llm_observability::sha256_hex_str(&brief),
                },
            )
            .await?;
        } else {
            crate::tools::batch_sql_runner::emit_batch_event(
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
    use crate::track_spec::TrackKind;
    use react_core::keyspace::{DefaultKeyspace, Keyspace};
    use react_core::llm::{ChatMessage, LargeLanguageModel};
    use react_core::scope::RequestScope;
    use react_core::storage::StorageAdapter;
    use react_module_storage_memory::InMemoryStorageAdapter;
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

    fn observed_customer_key_claim() -> Vec<crate::providers::SemanticClaimRef> {
        vec![crate::providers::SemanticClaimRef {
            claim_id: "candidate_key:test_raw.raw_customers:customer_id"
                .to_string()
                .into(),
            kind: crate::providers::SemanticClaimKind::CandidateKey,
            status: crate::providers::EvidenceStatus::Observed,
        }]
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
                "warehouse": { "kind": "athena", "container": "AwsDataCatalog", "namespace": "test_raw", "extras": {"region":"eu-west-1","workgroup":"wg","result_s3":"s3://x/"} },
                "catalog": { "enabled": false, "refresh_secs": 60, "max_concurrency": 8 },
                "dbt": { "enabled": true, "target": "athena", "naming": { "target_schema": "test", "silver_suffix": "silver", "gold_suffix": "gold" }, "runner": "host" },
                "vector": { "enabled": false }
            }),
        })
    }

    fn test_ctx(thread_id: &str) -> AgentCtx {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        let llm: Arc<dyn LargeLanguageModel> = Arc::new(ScriptedLlm {
            replies: Mutex::new(vec![]),
        });
        let warehouse: Arc<dyn crate::providers::WarehouseProvider> =
            Arc::new(crate::providers::warehouse::NullWarehouseProvider::default());
        let mut actx = react_core::agent::AgentCtxBuilder::new(
            llm,
            storage,
            RequestScope::parse("t", "w", "p").expect("valid test scope"),
            keyspace,
            Arc::new(react_core::agent::DefaultPolicy),
        )
        .top_k(1)
        .per_step_timeout_secs(1)
        .max_steps(2)
        .thread_id(thread_id.to_string())
        .agent_name("test".to_string())
        .resolved_config(Some(minimal_cfg()))
        .build();
        actx.set_capability(Arc::new(crate::ctx_ext::WarehouseCap(warehouse)));
        actx
    }

    async fn seed_cleanse_plan(ctx: &AgentCtx, sql_done: bool, schema_done: bool, locked: bool) {
        let mut checklist = plan::canonical_task_checklist(TrackKind::Cleanse);
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
            progress.consecutive_batch_failures =
                controller_kernel::max_consecutive_batch_failures();
        }
        let plan_doc = plan::CleansePlan {
            plan_key: plan::new_cleanse_plan_key(ctx),
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
        plan::save_cleanse_plan(ctx, &plan_doc).await.unwrap();
    }

    async fn seed_model_plan(ctx: &AgentCtx, sql_done: bool, schema_done: bool, locked: bool) {
        let stg_rel = "models/staging/stg_test_raw_raw_customers.sql";
        let stg_key = crate::project_fs::join_storage_key(ctx, stg_rel);
        ctx.storage()
            .put_bytes(
                &stg_key,
                b"select 1 as customer_id, 'x' as email",
                "text/sql",
            )
            .await
            .unwrap();

        let mut checklist = plan::canonical_task_checklist(TrackKind::Model);
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
            progress.consecutive_batch_failures =
                controller_kernel::max_consecutive_batch_failures();
        }
        let plan_doc = plan::ModelPlan {
            plan_key: plan::new_model_plan_key(ctx),
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
                source_schema: vec![plan::SourceColumnDef {
                    name: "customer_id".to_string(),
                    data_type: "bigint".to_string(),
                }],
                grounded_inputs: vec![plan::GroundedModelInput {
                    input_name: "stg_test_raw_raw_customers".to_string(),
                    model_rel_path: "models/staging/stg_test_raw_raw_customers.sql".to_string(),
                    relation_fqn: "catalog.db.stg_test_raw_raw_customers".to_string(),
                    source_schema: vec![plan::SourceColumnDef {
                        name: "customer_id".to_string(),
                        data_type: "bigint".to_string(),
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
        plan::save_model_plan(ctx, &plan_doc).await.unwrap();
    }

    #[tokio::test]
    async fn apply_next_cleanse_batch_contract_batch_locked() {
        let ctx = test_ctx("tid-cleanse-locked");
        seed_cleanse_plan(&ctx, false, false, true).await;
        let res = ApplyNextCleanseBatchTool { datasets: None }
            .call(serde_json::json!({}), &ctx)
            .await
            .unwrap();
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
    async fn apply_next_model_batch_contract_batch_locked() {
        let ctx = test_ctx("tid-model-locked");
        seed_model_plan(&ctx, false, false, true).await;
        let res = ApplyNextModelBatchTool
            .call(serde_json::json!({}), &ctx)
            .await
            .unwrap();
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
}
