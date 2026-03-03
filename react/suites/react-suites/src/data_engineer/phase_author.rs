use super::*;
use crate::data_engineer::control_flow::Phase;

impl DataEngineerSuite {
    pub(super) async fn execute_author_phase(
        thread_store: &ThreadStore,
        thread_id: &str,
        phase: crate::data_engineer::control_flow::Phase,
        question: &str,
        sctx: &SuiteCtx,
        execution_state: &crate::data_engineer::progress_controller::ExecutionState,
        guard: &crate::data_engineer::control_flow::DerivedGuardState,
        allow_ask_approval: bool,
        _thread_state_step_count: usize,
        last_validate_brief: &Option<String>,
        last_validate_failed_models: &[crate::data_engineer::progress_controller::FailedModelRef],
    ) -> Result<PhaseExecutorOutcome, String> {

let adapter = crate::data_engineer::authoring_driver::adapter_for_phase(phase)
    .ok_or_else(|| {
        format!(
            "authoring adapter missing for phase '{}'",
            phase.as_str()
        )
    })?;
let is_cleanse =
    adapter.kind() == crate::data_engineer::authoring_driver::AuthoringKind::Cleanse;
// Treat schema precheck failures as "validate failed" for authoring guard behavior.
// Otherwise we can bounce Author->Validate->Author without requiring a mutation.
let entered_from_precheck_failed = execution_state.phase_reason_code
    == Some(PhaseReasonCode::PrecheckFailed);
let mut phase_guard = guard.clone();
if entered_from_precheck_failed {
    phase_guard.last_validate_failed = true;
    phase_guard.mutated_since_fail = false;
}
let hard_mutation_repair_mode = execution_state.hard_mutation_repair_mode;
let mut repair_type = execution_state.repair_type;
if entered_from_precheck_failed {
    // Precheck-driven re-entry is always a schema repair path.
    repair_type = crate::data_engineer::progress_controller::RepairType::Schema;
}
let sys = crate::util::time_context::with_time_context(if is_cleanse {
    prompts::cleanse_system_prompt()
} else {
    prompts::model_system_prompt()
});
let authoring_ctx = crate::data_engineer::authoring_driver::AuthoringCtx {
    phase,
};
let mut actx = AgentCtx {
    top_k: 30,
    per_step_timeout_secs: 10,
    max_steps: 1,
    thread_id: Some(thread_id.to_string()),
    progress_tx: None,
    pre_step_tx: None,
    trace_tx: sctx.trace_tx.clone(),
    // IMPORTANT: always record a single agent label for agent-mode runs.
    // Phase selection (cleanse vs model) is handled by the deterministic outer loop and prompts.
    agent_name: Some("agent".to_string()),
    // IMPORTANT: in agent-mode, the deterministic outer loop enforces validation/invariants.
    // Non-interactive behavior is enforced at the suite boundary contract.
    policy: std::sync::Arc::new(InterruptOnlyPolicy),
    llm: sctx.llm.clone(),
    storage: sctx.storage.clone(),
    scope: sctx.scope.clone(),
    keyspace: sctx.keyspace.clone(),
    query: sctx.query.clone(),
    warehouse: sctx.warehouse.clone(),
    dbt: sctx.dbt.clone(),
    vector: sctx.vector.clone(),
    thread_store: Some(thread_store.clone()),
    exec_ctx: None,
    resolved_config: sctx.resolved_config.clone(),
};

// Plan-driven batching: load the approved plan, update progress from the thread log,
// and compute the exact next batch to execute (max 5).
let (plan_context, allowed_batch): (String, Option<AllowedBatch>) =
    if is_cleanse {
        let mut plan = match crate::data_engineer::plan::load_cleanse_plan_any(
            &actx,
        )
        .await
        {
            Some(p) => p,
            None => {
                // Recovery: authoring was entered, but no plan exists (e.g. restart/resume drift).
                // Bounce back to planning so the thread can rehydrate deterministically.
                apply_phase_transition(
                    &thread_store,
                    thread_id,
                    Some(phase),
                    Phase::CleansePlan,
                    control_flow::TransitionIntent::Loopback,
                    Some(PhaseReasonCode::PlanMissing),
                    Some(serde_json::json!({
                        "plan_kind": "cleanse",
                        "note": "authoring entered without an active cleanse plan; routing back to planning",
                    })),
                )
                .await?;
                return Ok(PhaseExecutorOutcome::Continue);
            }
        };
        if let control_flow::AuthoringGate::Block { reason } =
            control_flow::gate_author_phase_execution_cleanse(&plan)
        {
            let plan_key = plan.plan_key.clone();
            plan.status = crate::data_engineer::plan::PlanStatus::Cancelled;
            crate::data_engineer::plan::save_cleanse_plan(&actx, &plan)
                .await
                .map_err(|e| {
                    format!(
                        "failed to persist cancelled non-executable cleanse plan: {e}"
                    )
                })?;
            apply_guard_block(
                &thread_store,
                thread_id,
                phase,
                GuardBlockKind::PlanSemanticInvalid,
                reason.clone(),
            )
            .await?;
            apply_phase_transition(
                &thread_store,
                thread_id,
                Some(phase),
                Phase::CleansePlan,
                control_flow::TransitionIntent::Loopback,
                Some(PhaseReasonCode::PlanSemanticInvalid),
                Some(serde_json::json!({
                    "plan_key": plan_key,
                    "reason": reason,
                    "audit_acceptance": Self::churn_audit_acceptance_criteria(),
                })),
            )
            .await?;
            return Ok(PhaseExecutorOutcome::Continue);
        }
        // In deterministic repair mode, the plan is frozen (reference-only).
        // Normal progress is updated at tool-write time; do not reconstruct from thread logs.
        if !hard_mutation_repair_mode {
            let _ =
                crate::data_engineer::plan::save_cleanse_plan(&actx, &plan)
                    .await;
        }

        // Explicit execution context for hierarchical UI (best-effort).
        let next_item =
            crate::data_engineer::plan::cleanse_next_work_item_ctx(&plan);
        actx.exec_ctx = Some(react_core::session::ExecutionContext {
            plan_kind: Some(react_core::session::ExecutionPlanKind::new("cleanse")),
            plan_key: Some(plan.plan_key.clone()),
            workgroup_id: next_item.as_ref().map(|x| x.workgroup_id.clone()),
            task_id: next_item.as_ref().map(|x| x.task_id.clone()),
            checklist_item_id: next_item
                .as_ref()
                .map(|x| x.checklist_item_id.clone()),
            data: std::collections::BTreeMap::new(),
        });
        // Hard stop: if plan-batched authoring is locked due to too many consecutive failures,
        // return control to the user with a single actionable message (do not loop).
        if plan.progress.consecutive_batch_failures
        >= crate::data_engineer::controller_kernel::max_consecutive_batch_failures()
    {
        let next =
            crate::data_engineer::plan::cleanse_next_authoring_action(&plan)
                .author_sql_ids();
        let mut expected_paths: Vec<String> = Vec::new();
        for ds in next.iter() {
            if let Some(t) = plan.tasks.iter().find(|t| t.dataset_id == *ds) {
                if let Some(p) = t.expected_model_path.as_deref() {
                    if !p.trim().is_empty() {
                        expected_paths.push(p.trim().to_string());
                    }
                }
            }
        }
        expected_paths.sort();
        expected_paths.dedup();
        let reason = lock_prompt_for_plan(
            "cleanse",
            &plan.plan_key,
            plan.progress.consecutive_batch_failures,
            plan.progress.total_batch_failures,
            &next,
            &expected_paths,
        );
        apply_guard_block(
            &thread_store,
            thread_id,
            phase,
            GuardBlockKind::BatchLocked,
            reason.clone(),
        )
        .await?;
        return Err(batch_lock_error(&reason));
    }
        if plan.status != crate::data_engineer::plan::PlanStatus::Approved
            && plan.status != crate::data_engineer::plan::PlanStatus::Completed
        {
            apply_phase_transition(
            &thread_store,
            thread_id,
            Some(phase),
            Phase::CleansePlan,
            control_flow::TransitionIntent::Loopback,
            Some(PhaseReasonCode::PlanNotApproved),
            Some(serde_json::json!({ "status": format!("{:?}", plan.status) })),
        )
        .await?;
            return Ok(PhaseExecutorOutcome::Continue);
        }
        // Work-group driven selection only (hard cutover).
        let next_action =
            crate::data_engineer::plan::cleanse_next_authoring_action(&plan);
        let next = next_action.author_sql_ids();
        // IMPORTANT: If validation failed and we have not successfully mutated since,
        // the authoring tool registry will be patch-only (hard_mutation_only).
        // In that state, do NOT instruct apply_next_cleanse_batch; force repair-mode guidance.
        if hard_mutation_repair_mode
            && matches!(
                repair_type,
                crate::data_engineer::progress_controller::RepairType::SqlTarget
                    | crate::data_engineer::progress_controller::RepairType::Unknown
            )
        {
            // Repair-first routing: dbt_validate failed for a SQL/runtime-class reason.
            // Even if schema checklist work remains, fix failing SQL targets first.
            let mut ctx = format!(
                "Approved cleanse plan (stored at: {}).\nThe last dbt_validate failed and a mutating fix is required before any further validation.\n\nNext action: call file with a mutating op (patch|rm|mv) scoped to a repair target path.\n- If using patch: args.path + args.patch_text (Cursor/Aider hunks-only: '@@ ... @@'; no ---/+++ headers).\nExample args: {}\n\nRepair targets:\n",
                plan.plan_key,
                crate::data_engineer::patch_contract::single_file_patch_good_example_json()
            );
            if !last_validate_failed_models.is_empty() {
                for fm in last_validate_failed_models.iter().take(6) {
                    let name = if fm.name.trim().is_empty() {
                        "unknown_model"
                    } else {
                        fm.name.as_str()
                    };
                    let file = if fm.file.trim().is_empty() {
                        "(unknown file)"
                    } else {
                        fm.file.as_str()
                    };
                    ctx.push_str(&format!("- {} ({})\n", name, file));
                }
            } else if let Some(ref brief) = last_validate_brief {
                ctx.push_str("- (unknown failing model) — see last dbt_validate summary below.\n");
                ctx.push_str("\nLast dbt_validate summary:\n");
                ctx.push_str(brief);
                ctx.push('\n');
            } else {
                ctx.push_str("- (unknown failing model) — no failing-model evidence found.\n");
            }
            ctx.push_str("\nIMPORTANT: Defer any new checklist expansion or schema contract work until dbt_validate passes.\n");
            (ctx, None)
        } else if hard_mutation_repair_mode {
            // Schema/precheck failures: prefer schema batch tools when schema checklist work remains.
            if let crate::data_engineer::plan::AuthoringNextAction::AuthorSchema(ids) =
                &next_action
            {
                let checklist_item_id = actx
                    .exec_ctx
                    .as_ref()
                    .and_then(|c| c.checklist_item_id.as_deref())
                    .unwrap_or(
                        crate::data_engineer::plan::CHECKLIST_SCHEMA_CONTRACT,
                    )
                    .trim()
                    .to_string();
                let mut expected_paths: Vec<String> = Vec::new();
                for ds in ids.iter() {
                    if let Some(t) =
                        plan.tasks.iter().find(|t| t.dataset_id == *ds)
                    {
                        if let Some(p) = t.expected_model_path.as_deref() {
                            if !p.trim().is_empty() {
                                expected_paths.push(p.trim().to_string());
                            }
                        }
                    }
                }
                expected_paths.sort();
                expected_paths.dedup();
                let mut ctx = format!(
                    "Approved cleanse plan (stored at: {}).\nPending schema checklist work (checklist_item_id={} ; max 5):\n- {}\n\nNext action: call apply_next_cleanse_schema_batch (do NOT call file directly).\n\nExpected model SQL paths:\n- {}\n",
                    plan.plan_key,
                    checklist_item_id,
                    ids.join("\n- "),
                    expected_paths.join("\n- "),
                );
                ctx.push_str("\nIMPORTANT: Do NOT call the SQL batch-authoring tool while schema checklist work remains; continue schema checklist repairs first.\n");
                (ctx, None)
            } else {
                let mut ctx = format!(
                    "Approved cleanse plan (stored at: {}).\nThe last dbt_validate failed and a mutating fix is required before any further validation.\n\nRepair targets (fix these DBT files directly with file op=patch|rm|mv; if patching, use Cursor/Aider hunks-only patch_text).\nExample args: {}\n",
                    plan.plan_key,
                    crate::data_engineer::patch_contract::single_file_patch_good_example_json()
                );
                if !last_validate_failed_models.is_empty() {
                    for fm in last_validate_failed_models.iter().take(6) {
                        let name = if fm.name.trim().is_empty() {
                            "unknown_model"
                        } else {
                            fm.name.as_str()
                        };
                        let file = if fm.file.trim().is_empty() {
                            "(unknown file)"
                        } else {
                            fm.file.as_str()
                        };
                        ctx.push_str(&format!("- {} ({})\n", name, file));
                    }
                } else if let Some(ref brief) = last_validate_brief {
                    ctx.push_str("- (unknown failing model) — see last dbt_validate summary below.\n");
                    ctx.push_str("\nLast dbt_validate summary:\n");
                    ctx.push_str(brief);
                    ctx.push('\n');
                } else {
                    ctx.push_str("- (unknown failing model) — no failing-model evidence found.\n");
                }
                (ctx, None)
            }
        } else if next.is_empty() {
            // If work-groups exist, interpret "no next SQL batch" as:
            // - either we're blocked on schema checklist authoring, OR
            // - we're ready to transition to validate.
            if let crate::data_engineer::plan::AuthoringNextAction::AuthorSchema(ids) =
                &next_action
            {
                let checklist_item_id = actx
                    .exec_ctx
                    .as_ref()
                    .and_then(|c| c.checklist_item_id.as_deref())
                    .unwrap_or(
                        crate::data_engineer::plan::CHECKLIST_SCHEMA_CONTRACT,
                    )
                    .trim()
                    .to_string();
                let mut expected_paths: Vec<String> = Vec::new();
                for ds in ids.iter() {
                    if let Some(t) =
                        plan.tasks.iter().find(|t| t.dataset_id == *ds)
                    {
                        if let Some(p) = t.expected_model_path.as_deref() {
                            if !p.trim().is_empty() {
                                expected_paths.push(p.trim().to_string());
                            }
                        }
                    }
                }
                expected_paths.sort();
                expected_paths.dedup();
                let mut ctx = format!(
                    "Approved cleanse plan (stored at: {}).\nPending schema checklist work (checklist_item_id={} ; max 5):\n- {}\n\nNext action: call apply_next_cleanse_schema_batch (do NOT call file directly).\n\nExpected model SQL paths:\n- {}\n",
                    plan.plan_key,
                    checklist_item_id,
                    ids.join("\n- "),
                    expected_paths.join("\n- "),
                );
                ctx.push_str("\nIMPORTANT: Do NOT call the SQL batch-authoring tool while schema checklist work remains; continue schema checklist repairs first.\n");
                (
                    ctx,
                    Some(AllowedBatch::CleanseSchemaDatasetIds(ids.clone())),
                )
            } else {
                if matches!(
                    &next_action,
                    crate::data_engineer::plan::AuthoringNextAction::Validate
                ) {
                    apply_phase_transition(
                        &thread_store,
                        thread_id,
                        Some(phase),
                        Phase::CleanseValidate,
                        control_flow::TransitionIntent::Forward,
                        Some(PhaseReasonCode::WorkGroupValidate),
                        Some(serde_json::json!({ "plan_key": plan.plan_key })),
                    )
                    .await?;
                    return Ok(PhaseExecutorOutcome::Continue);
                }

                // If validation previously failed, do NOT bounce straight back to validate.
                // Run a repair authoring pass grounded in the failing model/file evidence.
                if guard.last_validate_failed {
                    let mut ctx = format!(
                "Approved cleanse plan (stored at: {}).\nAll plan tasks are currently marked done, but the last dbt_validate failed.\n\nRepair targets (fix these DBT files directly with file op=patch|rm|mv; if patching, use Cursor/Aider hunks-only patch_text).\nExample args: {}\n",
                    plan.plan_key,
                    crate::data_engineer::patch_contract::single_file_patch_good_example_json()
                );
                    if !last_validate_failed_models.is_empty() {
                        for fm in last_validate_failed_models.iter().take(6) {
                            let name = if fm.name.trim().is_empty() {
                                "unknown_model"
                            } else {
                                fm.name.as_str()
                            };
                            let file = if fm.file.trim().is_empty() {
                                "(unknown file)"
                            } else {
                                fm.file.as_str()
                            };
                            ctx.push_str(&format!("- {} ({})\n", name, file));
                        }
                    } else if let Some(ref brief) = last_validate_brief {
                        ctx.push_str("- (unknown failing model) — see last dbt_validate summary below.\n");
                        ctx.push_str("\nLast dbt_validate summary:\n");
                        ctx.push_str(brief);
                        ctx.push('\n');
                    } else {
                        ctx.push_str("- (unknown failing model) — no failing-model evidence found.\n");
                    }
                    (
                        ctx,
                        None, // allow freeform file patching for targeted repair
                    )
                } else if crate::data_engineer::plan::snapshot_cleanse_completion(&plan).all_done {
                    apply_phase_transition(
                        &thread_store,
                        thread_id,
                        Some(phase),
                        Phase::CleanseValidate,
                        control_flow::TransitionIntent::Forward,
                        Some(PhaseReasonCode::PlanTasksDone),
                        Some(serde_json::json!({ "plan_key": plan.plan_key })),
                    )
                    .await?;
                    return Ok(PhaseExecutorOutcome::Continue);
                } else {
                    let reason = "approved cleanse plan is not executable: no next work-group action while checklist work remains".to_string();
                    apply_guard_block(
                        &thread_store,
                        thread_id,
                        phase,
                        GuardBlockKind::PlanSemanticInvalid,
                        reason.clone(),
                    )
                    .await?;
                    apply_phase_transition(
                        &thread_store,
                        thread_id,
                        Some(phase),
                        Phase::CleansePlan,
                        control_flow::TransitionIntent::Loopback,
                        Some(PhaseReasonCode::PlanSemanticInvalid),
                        Some(serde_json::json!({ "plan_key": plan.plan_key, "reason": reason })),
                    )
                    .await?;
                    return Ok(PhaseExecutorOutcome::Continue);
                }
            }
        } else {
            (
            format!(
								"Approved cleanse plan (stored at: {}).\nNext batch (deterministic, max 5):\n- {}\n\nNext action: call the deterministic batch authoring tool from the current tool card (do NOT call staging_model directly).",
            plan.plan_key,
            next.join("\n- ")
            ),
            Some(AllowedBatch::CleanseSqlDatasetIds(next.clone())),
        )
        }
    } else {
        let mut plan = match crate::data_engineer::plan::load_model_plan_any(
            &actx,
        )
        .await
        {
            Some(p) => p,
            None => {
                // Recovery: authoring was entered, but no plan exists (e.g. restart/resume drift).
                // Bounce back to planning so the thread can rehydrate deterministically.
                apply_phase_transition(
                    &thread_store,
                    thread_id,
                    Some(phase),
                    Phase::ModelPlan,
                    control_flow::TransitionIntent::Loopback,
                    Some(PhaseReasonCode::PlanMissing),
                    Some(serde_json::json!({
                        "plan_kind": "model",
                        "note": "authoring entered without an active model plan; routing back to planning",
                    })),
                )
                .await?;
                return Ok(PhaseExecutorOutcome::Continue);
            }
        };
        if let control_flow::AuthoringGate::Block { reason } =
            control_flow::gate_author_phase_execution_model(&plan)
        {
            let plan_key = plan.plan_key.clone();
            plan.status = crate::data_engineer::plan::PlanStatus::Cancelled;
            let _ =
                crate::data_engineer::plan::save_model_plan(&actx, &plan).await;
            apply_guard_block(
                &thread_store,
                thread_id,
                phase,
                GuardBlockKind::PlanSemanticInvalid,
                reason.clone(),
            )
            .await?;
            apply_phase_transition(
                &thread_store,
                thread_id,
                Some(phase),
                Phase::ModelPlan,
                control_flow::TransitionIntent::Loopback,
                Some(PhaseReasonCode::PlanSemanticInvalid),
                Some(serde_json::json!({
                    "plan_key": plan_key,
                    "reason": reason,
                    "audit_acceptance": Self::churn_audit_acceptance_criteria(),
                })),
            )
            .await?;
            return Ok(PhaseExecutorOutcome::Continue);
        }
        // In deterministic repair mode, the plan is frozen (reference-only).
        // Normal progress is updated at tool-write time; do not reconstruct from thread logs.
        if !hard_mutation_repair_mode {
            let _ =
                crate::data_engineer::plan::save_model_plan(&actx, &plan)
                    .await;
        }

        // Explicit execution context for hierarchical UI (best-effort).
        let next_item =
            crate::data_engineer::plan::model_next_work_item_ctx(&plan);
        actx.exec_ctx = Some(react_core::session::ExecutionContext {
            plan_kind: Some(react_core::session::ExecutionPlanKind::new("model")),
            plan_key: Some(plan.plan_key.clone()),
            workgroup_id: next_item.as_ref().map(|x| x.workgroup_id.clone()),
            task_id: next_item.as_ref().map(|x| x.task_id.clone()),
            checklist_item_id: next_item
                .as_ref()
                .map(|x| x.checklist_item_id.clone()),
            data: std::collections::BTreeMap::new(),
        });
        if plan.progress.consecutive_batch_failures
        >= crate::data_engineer::controller_kernel::max_consecutive_batch_failures()
    {
        let next =
            crate::data_engineer::plan::model_next_authoring_action(&plan)
                .author_sql_ids();
        let mut expected_paths: Vec<String> = Vec::new();
        for n in next.iter() {
            if let Some(t) = plan.tasks.iter().find(|t| t.name == *n) {
                if let Some(p) = t.expected_model_path.as_deref() {
                    if !p.trim().is_empty() {
                        expected_paths.push(p.trim().to_string());
                    }
                }
            }
        }
        expected_paths.sort();
        expected_paths.dedup();
        let reason = lock_prompt_for_plan(
            "model",
            &plan.plan_key,
            plan.progress.consecutive_batch_failures,
            plan.progress.total_batch_failures,
            &next,
            &expected_paths,
        );
        apply_guard_block(
            &thread_store,
            thread_id,
            phase,
            GuardBlockKind::BatchLocked,
            reason.clone(),
        )
        .await?;
        return Err(batch_lock_error(&reason));
    }
        if plan.status != crate::data_engineer::plan::PlanStatus::Approved
            && plan.status != crate::data_engineer::plan::PlanStatus::Completed
        {
            apply_phase_transition(
            &thread_store,
            thread_id,
            Some(phase),
            Phase::ModelPlan,
            control_flow::TransitionIntent::Loopback,
            Some(PhaseReasonCode::PlanNotApproved),
            Some(serde_json::json!({ "status": format!("{:?}", plan.status) })),
        )
    .await?;
            return Ok(PhaseExecutorOutcome::Continue);
        }
        // Work-group driven selection only (hard cutover).
        let next_action =
            crate::data_engineer::plan::model_next_authoring_action(&plan);
        let next_names = next_action.author_sql_ids();
        // IMPORTANT: If validation failed and we have not successfully mutated since,
        // the authoring tool registry will be patch-only (hard_mutation_only).
        // In that state, do NOT instruct apply_next_model_batch; force repair-mode guidance.
        if hard_mutation_repair_mode
            && matches!(
                repair_type,
                crate::data_engineer::progress_controller::RepairType::SqlTarget
                    | crate::data_engineer::progress_controller::RepairType::Unknown
            )
        {
            // Repair-first routing: dbt_validate failed for a SQL/runtime-class reason.
            // Even if schema checklist work remains, fix failing SQL targets first.
            let mut ctx = format!(
                "Approved model plan (stored at: {}).\nThe last dbt_validate failed and a mutating fix is required before any further validation.\n\nNext action: call file with a mutating op (patch|rm|mv) scoped to a repair target path.\n- If using patch: args.path + args.patch_text (Cursor/Aider hunks-only: '@@ ... @@'; no ---/+++ headers).\nExample args: {}\n\nRepair targets:\n",
                plan.plan_key,
                crate::data_engineer::patch_contract::single_file_patch_good_example_json()
            );
            if !last_validate_failed_models.is_empty() {
                for fm in last_validate_failed_models.iter().take(6) {
                    let name = if fm.name.trim().is_empty() {
                        "unknown_model"
                    } else {
                        fm.name.as_str()
                    };
                    let file = if fm.file.trim().is_empty() {
                        "(unknown file)"
                    } else {
                        fm.file.as_str()
                    };
                    ctx.push_str(&format!("- {} ({})\n", name, file));
                }
            } else if let Some(ref brief) = last_validate_brief {
                ctx.push_str("- (unknown failing model) — see last dbt_validate summary below.\n");
                ctx.push_str("\nLast dbt_validate summary:\n");
                ctx.push_str(brief);
                ctx.push('\n');
            } else {
                ctx.push_str("- (unknown failing model) — no failing-model evidence found.\n");
            }
            ctx.push_str("\nIMPORTANT: Defer any new checklist expansion or schema contract work until dbt_validate passes.\n");
            (ctx, None)
        } else if hard_mutation_repair_mode {
            if let crate::data_engineer::plan::AuthoringNextAction::AuthorSchema(ids) =
                &next_action
            {
                let checklist_item_id = actx
                    .exec_ctx
                    .as_ref()
                    .and_then(|c| c.checklist_item_id.as_deref())
                    .unwrap_or(
                        crate::data_engineer::plan::CHECKLIST_SCHEMA_CONTRACT,
                    )
                    .trim()
                    .to_string();
                let mut expected_paths: Vec<String> = Vec::new();
                for n in ids.iter() {
                    if let Some(t) = plan.tasks.iter().find(|t| t.name == *n) {
                        if let Some(p) = t.expected_model_path.as_deref() {
                            if !p.trim().is_empty() {
                                expected_paths.push(p.trim().to_string());
                            }
                        }
                    }
                }
                expected_paths.sort();
                expected_paths.dedup();
                let mut ctx = format!(
                    "Approved model plan (stored at: {}).\nPending schema checklist work (checklist_item_id={} ; max 5):\n- {}\n\nNext action: call apply_next_model_schema_batch (do NOT call file directly).\n\nExpected model SQL paths:\n- {}\n",
                    plan.plan_key,
                    checklist_item_id,
                    ids.join("\n- "),
                    expected_paths.join("\n- "),
                );
                ctx.push_str("\nIMPORTANT: Do NOT call the SQL batch-authoring tool while schema checklist work remains; continue schema checklist repairs first.\n");
                (
                    ctx,
                    Some(AllowedBatch::ModelSchemaItemNames(ids.clone())),
                )
            } else {
                let mut ctx = format!(
                    "Approved model plan (stored at: {}).\nThe last dbt_validate failed and a mutating fix is required before any further validation.\n\nRepair targets (fix these DBT files directly with file op=patch|rm|mv; if patching, use Cursor/Aider hunks-only patch_text).\nExample args: {}\n",
                    plan.plan_key,
                    crate::data_engineer::patch_contract::single_file_patch_good_example_json()
                );
                if !last_validate_failed_models.is_empty() {
                    for fm in last_validate_failed_models.iter().take(6) {
                        let name = if fm.name.trim().is_empty() {
                            "unknown_model"
                        } else {
                            fm.name.as_str()
                        };
                        let file = if fm.file.trim().is_empty() {
                            "(unknown file)"
                        } else {
                            fm.file.as_str()
                        };
                        ctx.push_str(&format!("- {} ({})\n", name, file));
                    }
                } else if let Some(ref brief) = last_validate_brief {
                    ctx.push_str("- (unknown failing model) — see last dbt_validate summary below.\n");
                    ctx.push_str("\nLast dbt_validate summary:\n");
                    ctx.push_str(brief);
                    ctx.push('\n');
                } else {
                    ctx.push_str("- (unknown failing model) — no failing-model evidence found.\n");
                }
                (ctx, None)
            }
        } else if next_names.is_empty() {
            if let crate::data_engineer::plan::AuthoringNextAction::AuthorSchema(ids) =
                &next_action
            {
                // Deterministic pre-check: if models/schema.yml already contains model stanzas
                // for these pending items, mark schema_contract done and re-run planning for the
                // next action instead of thrashing the same file.
                {
                    let key = crate::data_engineer::files_store::join_storage_key(
                        &actx,
                        crate::data_engineer::project_files::MODELS_SCHEMA_YML,
                    );
                    if let Ok(bytes) = actx.storage.get_bytes(&key).await {
                        let content =
                            String::from_utf8_lossy(&bytes).to_string();
                        if let Ok(vy) =
                            serde_yaml::from_str::<serde_yaml::Value>(&content)
                        {
                            let mut names_in_schema: std::collections::HashSet<
                                String,
                            > = std::collections::HashSet::new();
                            if let Some(models) =
                                vy.get("models").and_then(|m| m.as_sequence())
                            {
                                for m in models.iter() {
                                    if let Some(nm) = m
                                        .get("name")
                                        .and_then(|n| n.as_str())
                                        .map(|s| s.trim().to_string())
                                        .filter(|s| !s.is_empty())
                                    {
                                        names_in_schema.insert(nm);
                                    }
                                }
                            }
                            let mut changed = false;
                            let checklist_item_id = actx
                                .exec_ctx
                                .as_ref()
                                .and_then(|c| c.checklist_item_id.as_deref())
                                .unwrap_or(
                                    crate::data_engineer::plan::CHECKLIST_SCHEMA_CONTRACT,
                                )
                                .trim()
                                .to_string();
                            for n in ids.iter() {
                                if !names_in_schema.contains(n) {
                                    return Ok(PhaseExecutorOutcome::Continue);
                                }
                                if let Some(t) =
                                    plan.tasks.iter().find(|t| t.name == *n)
                                {
                                    let done = t
                                        .checklist
                                        .iter()
                                        .find(|it| it.checklist_item_id == checklist_item_id)
                                        .map(|it| {
                                            it.status
                                                == crate::data_engineer::plan::ChecklistItemStatus::Done
                                        })
                                        .unwrap_or(false);
                                    if !done {
                                        changed = true;
                                    }
                                }
                                crate::data_engineer::plan::model_checklist_mark_status(
                                    &mut plan,
                                    n,
                                    &checklist_item_id,
                                    crate::data_engineer::plan::ChecklistItemStatus::Done,
                                );
                            }
                            if changed {
                                crate::data_engineer::plan::save_model_plan(
                                    &actx, &plan,
                                )
                                .await?;
                                return Ok(PhaseExecutorOutcome::Continue);
                            }
                        }
                    }
                }

                let mut expected_paths: Vec<String> = Vec::new();
                for n in ids.iter() {
                    if let Some(t) = plan.tasks.iter().find(|t| t.name == *n) {
                        if let Some(p) = t.expected_model_path.as_deref() {
                            if !p.trim().is_empty() {
                                expected_paths.push(p.trim().to_string());
                            }
                        }
                    }
                }
                expected_paths.sort();
                expected_paths.dedup();
                let checklist_item_id = actx
                    .exec_ctx
                    .as_ref()
                    .and_then(|c| c.checklist_item_id.as_deref())
                    .unwrap_or(
                        crate::data_engineer::plan::CHECKLIST_SCHEMA_CONTRACT,
                    )
                    .trim()
                    .to_string();
                let mut ctx = format!(
                    "Approved model plan (stored at: {}).\nPending schema checklist work (checklist_item_id={} ; max 5):\n- {}\n\nNext action: call apply_next_model_schema_batch (do NOT call file directly).\n\nExpected model SQL paths:\n- {}\n",
                    plan.plan_key,
                    checklist_item_id,
                    ids.join("\n- "),
                    expected_paths.join("\n- "),
                );
                ctx.push_str("\nIMPORTANT: Do NOT call the SQL batch-authoring tool while schema checklist work remains; continue schema checklist repairs first.\n");
                (ctx, None)
            } else {
                if matches!(
                    &next_action,
                    crate::data_engineer::plan::AuthoringNextAction::Validate
                ) {
                    apply_phase_transition(
                        &thread_store,
                        thread_id,
                        Some(phase),
                        Phase::ModelValidate,
                        control_flow::TransitionIntent::Forward,
                        Some(PhaseReasonCode::WorkGroupValidate),
                        Some(serde_json::json!({ "plan_key": plan.plan_key })),
                    )
                    .await?;
                    return Ok(PhaseExecutorOutcome::Continue);
                }

                if guard.last_validate_failed {
                    // Same repair-mode behavior as cleanse: run authoring to patch failing files.
                    let mut ctx = format!(
                "Approved model plan (stored at: {}).\nAll plan tasks are currently marked done, but the last dbt_validate failed.\n\nRepair targets (fix these DBT files directly with file op=patch|rm|mv; if patching, use Cursor/Aider hunks-only patch_text).\nExample args: {}\n",
                plan.plan_key,
                crate::data_engineer::patch_contract::single_file_patch_good_example_json()
            );
                    if !last_validate_failed_models.is_empty() {
                        for fm in last_validate_failed_models.iter().take(6) {
                            let name = if fm.name.trim().is_empty() {
                                "unknown_model"
                            } else {
                                fm.name.as_str()
                            };
                            let file = if fm.file.trim().is_empty() {
                                "(unknown file)"
                            } else {
                                fm.file.as_str()
                            };
                            ctx.push_str(&format!("- {} ({})\n", name, file));
                        }
                    } else if let Some(ref brief) = last_validate_brief {
                        ctx.push_str("- (unknown failing model) — see last dbt_validate summary below.\n");
                        ctx.push_str("\nLast dbt_validate summary:\n");
                        ctx.push_str(brief);
                        ctx.push('\n');
                    } else {
                        ctx.push_str("- (unknown failing model) — no failing-model evidence found.\n");
                    }
                    (ctx, None)
                } else if crate::data_engineer::plan::snapshot_model_completion(&plan).all_done {
                    apply_phase_transition(
                        &thread_store,
                        thread_id,
                        Some(phase),
                        Phase::ModelValidate,
                        control_flow::TransitionIntent::Forward,
                        Some(PhaseReasonCode::PlanTasksDone),
                        Some(serde_json::json!({ "plan_key": plan.plan_key })),
                    )
                    .await?;
                    return Ok(PhaseExecutorOutcome::Continue);
                } else {
                    let reason = "approved model plan is not executable: no next work-group action while checklist work remains".to_string();
                    apply_guard_block(
                        &thread_store,
                        thread_id,
                        phase,
                        GuardBlockKind::PlanSemanticInvalid,
                        reason.clone(),
                    )
                    .await?;
                    apply_phase_transition(
                    &thread_store,
                    thread_id,
                    Some(phase),
                    Phase::ModelPlan,
                    control_flow::TransitionIntent::Loopback,
                    Some(PhaseReasonCode::PlanSemanticInvalid),
                    Some(serde_json::json!({ "plan_key": plan.plan_key, "reason": reason })),
                )
                .await?;
                    return Ok(PhaseExecutorOutcome::Continue);
                }
            }
        } else {
            let allowed =
                Some(AllowedBatch::ModelSqlItemNames(next_names.clone()));
            // Include task details for the next batch so the LLM can call gold_model with full args.
            let mut details: Vec<String> = Vec::new();
            for n in next_names.iter() {
                if let Some(t) = plan.tasks.iter().find(|t| t.name == *n) {
                    details.push(format!(
                        "- name: {}\n  folder: {}\n  goal: {}\n  inputs: {:?}",
                        t.name, t.folder, t.goal, t.inputs
                    ));
                } else {
                    details.push(format!("- name: {}", n));
                }
            }
            (
            format!(
								"Approved model plan (stored at: {}).\nNext batch (deterministic, max 5):\n{}\n\nNext action: call the deterministic batch authoring tool from the current tool card (do NOT call gold_model directly).",
            plan.plan_key,
            details.join("\n")
            ),
            allowed,
        )
        }
    };

let single_target_repair_path = if hard_mutation_repair_mode {
    Self::derive_single_target_repair_path(
        &execution_state,
        &last_validate_failed_models,
    )
} else {
    None
};
let (registry, tools_card) = Self::build_tools_for_phase(
    phase,
    &phase_guard,
    allow_ask_approval,
    sctx,
    allowed_batch.clone(),
    single_target_repair_path.clone(),
    false,
)?;

// Ground the next authoring pass with last validation summary (if any) and guard state.
let mut q = if is_cleanse {
    Self::inject_cleanse_question(question)
} else {
    Self::inject_model_question(question)
};
if !is_cleanse {
    // Ground gold authoring with the current silver inventory so the agent
    // can reliably build marts from existing stg_* models (no guessing).
    let base = actx
        .keyspace
        .dbt_prefix(&actx.scope)
        .trim_end_matches('/')
        .to_string();
    let pref = format!("{}/models/staging/", base);
    if let Ok(keys) = actx.storage.list_prefix(&pref).await {
        let mut rels: Vec<String> = keys
            .into_iter()
            .filter(|k| k.ends_with(".sql") && !k.contains("/_versions/"))
            .filter_map(|k| {
                k.strip_prefix(&(base.clone() + "/")).map(|s| s.to_string())
            })
            .collect();
        rels.sort();
        rels.dedup();
        let mut names: Vec<String> = rels
            .into_iter()
            .filter_map(|rel| {
                std::path::Path::new(&rel)
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
            })
            .collect();
        names.sort();
        names.dedup();
        if !names.is_empty() {
            q.push_str("\n\nCurrent staged silver models (use ref('stg_*') from these):\n");
            for n in names.into_iter().take(60) {
                q.push_str("- ");
                q.push_str(&n);
                q.push('\n');
            }
        }
    }
}
q.push_str("\n\nNOTE: In agent mode, validation and publish are handled by the suite phases. Do not call dbt_validate or publish tools; focus on authoring fixes and models.");
q.push_str("\nIMPORTANT: Tool-call argument shapes are strict. In particular: vect_query uses args.query_text (NOT args.query) and scope must be \"dataset\"|\"field\"|\"doc\"|\"artifact\"|\"metric\"|\"model\".");
q.push_str("\nIMPORTANT: sql_stats and sql_sample both require args.field. To sample rows, use run_sql with a LIMIT.");
q.push_str("\nIMPORTANT: This authoring phase is plan-driven. Follow the Plan context below. If it says to patch failing DBT files, do that first; if it provides a next batch, execute it. Do NOT ask for approval; approvals happen in plan phases.");
q.push_str("\nIMPORTANT: No downstream compensation exists for incomplete plan structure. If execution context is incomplete, return to planning; do not invent fallback execution.");
q.push_str("\n\nPlan context:\n");
q.push_str(&plan_context);
// Auto-attach authoritative schema facts (no ambiguity) for this phase.
// - In authoring, include ALL relations in the current approved batch.
// - Also include any recent validate-fail facts snapshot if present.
{
    let dialect = crate::config::resolved_config_from_ctx(&actx)
        .as_ref()
        .map(|cfg| crate::data_engineer::dbt_repair::remediate::active_provider_dialect(cfg))
        .unwrap_or_else(|| "Unknown SQL dialect".to_string());
    let mut batch_relations: Vec<String> = Vec::new();
    let mut prior_validate_facts: Option<serde_json::Value> = None;
    if is_cleanse {
        if let Some(p) =
            crate::data_engineer::plan::load_cleanse_plan(&actx).await
        {
            if let Some(ab) = allowed_batch.as_ref() {
                if let AllowedBatch::CleanseSqlDatasetIds(ds)
                | AllowedBatch::CleanseSchemaDatasetIds(ds) = ab
                {
                    batch_relations =
                        crate::data_engineer::facts::dataset_ids_to_fqns(ds);
                }
            }
            // Prefer the last persisted validate_fail_facts snapshot (if any).
            if let Some(obj) = p.project_snapshot.as_object() {
                if let Some(arr) =
                    obj.get("validate_fail_facts").and_then(|v| v.as_array())
                {
                    if let Some(last) = arr.last() {
                        prior_validate_facts = Some(last.clone());
                    }
                }
            }
        }
    } else {
        if let Some(p) =
            crate::data_engineer::plan::load_model_plan(&actx).await
        {
            if let Some(ab) = allowed_batch.as_ref() {
                if let AllowedBatch::ModelSqlItemNames(names)
                | AllowedBatch::ModelSchemaItemNames(names) = ab
                {
                    // Include relations for the models in the batch AND their declared inputs.
                    let mut want_names: Vec<String> = names.clone();
                    for n in names.iter() {
                        if let Some(t) = p.tasks.iter().find(|t| t.name == *n) {
                            for inp in t.inputs.iter() {
                                let s = inp.trim();
                                if !s.is_empty() {
                                    want_names.push(s.to_string());
                                }
                            }
                        }
                    }
                    want_names.sort();
                    want_names.dedup();
                    batch_relations =
                        crate::data_engineer::facts::resolve_model_names_to_fqns(&actx, &want_names).await;
                }
            }
            if let Some(obj) = p.project_snapshot.as_object() {
                if let Some(arr) =
                    obj.get("validate_fail_facts").and_then(|v| v.as_array())
                {
                    if let Some(last) = arr.last() {
                        prior_validate_facts = Some(last.clone());
                    }
                }
            }
        }
    }

    if !batch_relations.is_empty() {
        let limits = crate::data_engineer::facts::FactsLimits::for_scope(
            crate::data_engineer::facts::FactsScope::AuthorBatch,
        );
        let bundle =
            crate::data_engineer::facts::build_facts_bundle_from_relations(
                &actx,
                crate::data_engineer::facts::FactsScope::AuthorBatch,
                dialect.clone(),
                crate::data_engineer::facts::TargetFacts::default(),
                &batch_relations,
                limits,
            )
            .await;
        q.push_str("\n\nIMMUTABLE FACTS (author_batch_schema):\n");
        q.push_str(
            &serde_json::to_string_pretty(&bundle)
                .unwrap_or_else(|_| "{}".to_string()),
        );
        q.push('\n');
        q.push_str("Rules:\n- You MUST NOT reference any column not present in facts.relations[].columns for that relation.\n- If required facts are missing, call sql_schema and then patch.\n");
    }
    if let Some(vf) = prior_validate_facts {
        q.push_str("\n\nIMMUTABLE FACTS (latest_validate_fail_facts):\n");
        q.push_str(
            &serde_json::to_string_pretty(&vf)
                .unwrap_or_else(|_| "{}".to_string()),
        );
        q.push('\n');
    }
}
if let Some(b) = last_validate_brief.as_ref() {
    q.push_str("\n\nLast dbt_validate summary (most recent):\n");
    q.push_str(b);
}
if !last_validate_failed_models.is_empty() {
    q.push_str("\n\nFailing DBT model targets (from dbt stdout):\n");
    for fm in last_validate_failed_models.iter().take(6) {
        let name = if fm.name.trim().is_empty() {
            "unknown_model"
        } else {
            fm.name.as_str()
        };
        let file = if fm.file.trim().is_empty() {
            "(unknown file)"
        } else {
            fm.file.as_str()
        };
        q.push_str("- ");
        q.push_str(name);
        q.push_str(" (");
        q.push_str(file);
        q.push_str(")\n");
    }
    q.push_str("Fix these first (prefer patching the listed file paths).\n");
}
if let Some(ref target) = single_target_repair_path {
    q.push_str("\n\nDETERMINISTIC SINGLE-TARGET REPAIR MODE:\n");
    q.push_str("- You MUST mutate ONLY this file path in your next mutation (file op=patch|rm|mv):\n");
    q.push_str("- ");
    q.push_str(target);
    q.push_str("\n- Do NOT patch, move, or remove any other file until this target validates.\n");
}

// When the suite is in hard_mutation_only, file is patch-only (no op=get),
// so we MUST include the raw file content for at least the primary failing target.
if hard_mutation_repair_mode && !last_validate_failed_models.is_empty() {
    if let Some(file) = Some(last_validate_failed_models[0].file.as_str()) {
        let file = file.trim();
        if !file.is_empty() && file != "(unknown file)" {
            let base = actx
                .keyspace
                .dbt_prefix(&actx.scope)
                .trim_end_matches('/')
                .to_string();
            let key = format!("{}/{}", base, file);
            if let Ok(bytes) = actx.storage.get_bytes(&key).await {
                let content = String::from_utf8_lossy(&bytes).to_string();
                let fence_lang = if file.ends_with(".yml") || file.ends_with(".yaml") {
                    "yaml"
                } else {
                    "sql"
                };
                q.push_str("\n\nPrimary repair target current file content:\n");
                q.push_str("File: ");
                q.push_str(file);
                q.push_str("\n\n```");
                q.push_str(fence_lang);
                q.push_str("\n");
                q.push_str(&content);
                if !content.ends_with('\n') {
                    q.push('\n');
                }
                q.push_str("```\n");
            }
        }
    }
}
// Surface the latest typed suite-level error note (if any) to help auto-fix.
if let Some(reason) = execution_state
    .last_error_brief
    .as_ref()
    .map(|s| s.trim())
    .filter(|s| !s.is_empty())
{
    q.push_str("\n\nSuite guard note (must resolve before validate):\n");
    q.push_str(reason);
}
// If we re-entered authoring due to review feedback, inject the full review text (by ref)
// so the agent can address it in implementation without reopening the plan.
if execution_state.phase_reason_code == Some(PhaseReasonCode::ReviewPatchImpl) {
    if let Some(key) = execution_state
        .phase_reason_detail
        .as_ref()
        .and_then(|v| v.get("meta"))
        .and_then(|v| v.get("review_ref"))
        .and_then(|v| v.get("key"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        if let Ok(bytes) = actx.storage.get_bytes(&key).await {
            let txt = String::from_utf8_lossy(&bytes).to_string();
            if !txt.trim().is_empty() {
                q.push_str("\n\nPRIOR REVIEW FEEDBACK (must address by editing implementation; do NOT change the approved plan/spec):\n");
                q.push_str(txt.trim());
                q.push('\n');
            }
        }
    }
}
if hard_mutation_repair_mode {
    q.push_str("\n\nConstraint: your next steps must APPLY A MUTATING FIX before attempting dbt_validate again.");
}
if phase_guard.probe_required && !phase_guard.probe_satisfied {
    q.push_str("\n\nConstraint: runtime validation failed after compile; run meaningful run_sql probes (not SELECT 1) to diagnose data before re-validating. Multiple probes are allowed while they add new signal; repeated same/no-signal probes require you to switch to a mutating file fix.");
}

// Deterministic invariant: do not allow leaving authoring without any models.
let has_models = control_flow::invariant_has_any_models(&actx)
    .await
    .unwrap_or(false);
if !has_models {
    q.push_str("\n\nIMPORTANT: invariant failed: there are no DBT model SQL files yet. Your first task is to create at least one staging model under models/ using staging_model or file op=patch.");
}

// Hard cutover: deterministic repair-mode mini-context.
// When validate failed and we are in single-target repair mode, do NOT feed the model the full
// accumulated authoring prompt (plan context, immutable facts, review text, etc).
// Instead, provide a minimal packet: target file + failure brief + strict allowed operation.
if hard_mutation_repair_mode
    && matches!(
        repair_type,
        crate::data_engineer::progress_controller::RepairType::SqlTarget
            | crate::data_engineer::progress_controller::RepairType::Unknown
    )
    && single_target_repair_path.as_ref().is_some()
{
    let target = single_target_repair_path
        .as_ref()
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let mut es = crate::data_engineer::progress_controller::ExecutionState::load(
        &thread_store,
        thread_id,
    )
    .await
    .unwrap_or_else(
        crate::data_engineer::progress_controller::ExecutionState::new,
    );
    if es.target_path.as_deref().unwrap_or("").trim().is_empty()
        && !target.is_empty()
    {
        es.target_path = Some(target.clone());
        es.save(&thread_store, thread_id).await.map_err(|e| {
            format!(
                "failed to persist execution-state target path in deterministic repair mode: {e}"
            )
        })?;
    }
    let ladder = es.ladder_step.clone();

    let mut content = String::new();
    if !target.is_empty() {
        let base = actx
            .keyspace
            .dbt_prefix(&actx.scope)
            .trim_end_matches('/')
            .to_string();
        let key = format!("{}/{}", base, target);
        if let Ok(bytes) = actx.storage.get_bytes(&key).await {
            content = String::from_utf8_lossy(&bytes).to_string();
        }
    }

    let envelope = crate::data_engineer::prompt_packets::PromptEnvelope {
        phase: phase.as_str().to_string(),
        goal: question.trim().to_string(),
        directive: crate::data_engineer::prompt_packets::TurnDirective::Repair,
        plan: None,
        batch: None,
        repair: Some(crate::data_engineer::prompt_packets::RepairPacket {
            target_path: target.clone(),
            ladder_step: ladder.clone(),
            last_validate_brief: last_validate_brief.clone(),
            patch_contract: Some(
                crate::prompts::patch_contract::file_patch_contract()
                    .to_string(),
            ),
        }),
    };
    let mut repair =
        crate::data_engineer::prompt_packets::render_envelope(&envelope)
            .map_err(|e| format!("invalid repair prompt envelope: {e}"))?;

    repair.push_str("\nRules:\n");
    repair.push_str("- You MUST call file with a mutating op next (patch|rm|mv).\n");
    repair.push_str("- You MUST mutate ONLY the target file above.\n");
    repair.push_str("- Do NOT use placeholder patch headers like '@@ ... @@'; use real hunks with exact context from the current file content.\n");
    if target.ends_with(".yml") || target.ends_with(".yaml") {
        repair.push_str("- YAML repair rule: edit existing keys in place; do NOT append duplicate top-level keys like 'version:' or 'models:'.\n");
    }
    match ladder {
        crate::data_engineer::progress_controller::RepairLadderStep::ReplaceFile => {
            repair.push_str("- IMPORTANT: prefer a guarded single-file patch (args.path + args.patch_text; hunks-only). If patching cannot converge, you may use rm/mv but only against the target path.\n");
        }
        crate::data_engineer::progress_controller::RepairLadderStep::Stop => {
            repair.push_str("- STOP: prior repair attempts did not converge. Do not continue.\n");
        }
        crate::data_engineer::progress_controller::RepairLadderStep::PatchTarget => {}
    }

    let fence_lang = if target.ends_with(".yml") || target.ends_with(".yaml") {
        "yaml"
    } else {
        "sql"
    };
    repair.push_str("\nCurrent target file content:\n```");
    repair.push_str(fence_lang);
    repair.push_str("\n");
    repair.push_str(&content);
    if !content.ends_with('\n') {
        repair.push('\n');
    }
    repair.push_str("```\n");

    q = repair;
}

let llm_options = if is_cleanse {
    let author_max_tokens: u32 = std::env::var("LLM_AUTHOR_MAX_TOKENS_CLEANSE")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(24_000)
        .max(2_000)
        .min(64_000);
    LlmCallOptions {
        prompt_id: "data_engineer.cleanse_author",
        thread_id: None,
        expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
        temperature: Some(0.05),
        top_p: Some(1.0),
        max_output_tokens: Some(author_max_tokens),
        reasoning_effort: None,
    }
} else {
    let author_max_tokens: u32 = std::env::var("LLM_AUTHOR_MAX_TOKENS_MODEL")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(32_000)
        .max(2_000)
        .min(64_000);
    LlmCallOptions {
        prompt_id: "data_engineer.model_author",
        thread_id: None,
        expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
        temperature: Some(0.12),
        top_p: Some(1.0),
        max_output_tokens: Some(author_max_tokens),
        reasoning_effort: None,
    }
};
let pre_mutation_epoch = execution_state.mutation_epoch;
match Agent::run_until_block_non_interactive(
    &registry,
    &actx,
    &sys,
    &tools_card,
    &q,
    llm_options,
)
.await
{
    Ok(RunOutcomeNonInteractive::Final { .. }) => {
        // Progress is updated at tool-write time; avoid thread-log replay for state.

        // Deterministic invariants: don't advance phases unless the project actually exists.
        let has_proj = control_flow::invariant_has_dbt_project(&actx)
            .await
            .unwrap_or(false);
        let has_models = control_flow::invariant_has_any_models(&actx)
            .await
            .unwrap_or(false);
        if !has_proj || !has_models {
            // Stay in the same authoring phase; the next pass will be prompted with invariant context.
            return Ok(PhaseExecutorOutcome::Continue);
        }
        // Hard cutover: single progress gate controls authoring->validate advancement.
        // Refresh control-state after tool run; tool-side writes in this turn
        // must be visible before we decide whether authoring can advance.
        let gate_state = crate::data_engineer::progress_controller::ExecutionState::load_strict(
            &thread_store,
            thread_id,
        )
        .await?
        .unwrap_or_else(crate::data_engineer::progress_controller::ExecutionState::new);
        if let Err(reason) = crate::data_engineer::progress_controller::gate_authoring_progress(
            &gate_state,
            phase,
        ) {
            apply_guard_block(
                &thread_store,
                thread_id,
                phase,
                GuardBlockKind::AuthoringToValidate,
                reason.clone(),
            )
            .await?;
            return Ok(PhaseExecutorOutcome::Continue);
        }
        if Self::patch_impl_intent_unsatisfied(&gate_state, phase) {
            let reason = format!(
                "progress_gate_blocked: review requested implementation patch for phase '{}' and no successful mutation has been recorded since loopback. Apply a mutating file op (patch/rm/mv) before re-validating.",
                phase.as_str()
            );
            apply_guard_block(
                &thread_store,
                thread_id,
                phase,
                GuardBlockKind::AuthoringToValidate,
                reason.clone(),
            )
            .await?;
            return Ok(PhaseExecutorOutcome::Continue);
        }
        if matches!(
            gate_state.pending_loopback_intent.as_ref(),
            Some(crate::data_engineer::progress_controller::PendingLoopbackIntent::PatchImpl { phase: p, .. }) if *p == phase
        ) {
            let _ =
                Self::clear_pending_loopback_intent(&thread_store, thread_id)
                    .await;
        }

        // Model authoring must actually produce at least one gold model SQL file.
        // Without this, we can "succeed" in silver but never create any gold schema objects.
        if !is_cleanse {
            let actx = Self::agent_tool_ctx(thread_id, sctx);
            if !Self::has_any_gold_model_sql(&actx).await {
                let reason = "No gold models were found under models/core/ or models/marts/ after ModelAuthor. Gold must be explicitly authored (marts/core SQL) before validating/publishing.";
                apply_guard_block(
                    &thread_store,
                    thread_id,
                    phase,
                    GuardBlockKind::MissingGoldModels,
                    reason.to_string(),
                )
                .await?;
                // Hard cutover: same-phase blocks are represented as GuardBlock only.
                return Ok(PhaseExecutorOutcome::Continue);
            }
        }

        // Plan-driven authoring: do NOT advance to validate until the approved plan's tasks are done.
        if is_cleanse {
            if let Some(p) =
                crate::data_engineer::plan::load_cleanse_plan(&actx).await
            {
                if !crate::data_engineer::plan::snapshot_cleanse_completion(&p)
                    .all_done
                {
                    return Ok(PhaseExecutorOutcome::Continue);
                }
            }
        } else {
            if let Some(p) =
                crate::data_engineer::plan::load_model_plan(&actx).await
            {
                if !crate::data_engineer::plan::snapshot_model_completion(&p)
                    .all_done
                {
                    return Ok(PhaseExecutorOutcome::Continue);
                }
            }
        }

        let to_phase = if is_cleanse {
            Phase::CleanseValidate
        } else {
            Phase::ModelValidate
        };
        let reason_detail = Self::authoring_complete_reason_detail(
            &thread_store,
            thread_id,
            has_proj,
            has_models,
        )
        .await;
        apply_phase_transition(
            &thread_store,
            thread_id,
            Some(phase),
            to_phase,
            control_flow::TransitionIntent::Forward,
            Some(PhaseReasonCode::AuthoringComplete),
            Some(reason_detail),
        )
        .await?;
        return Ok(PhaseExecutorOutcome::Continue);
    }
    Ok(RunOutcomeNonInteractive::StepBoundary { .. }) => {
        if hard_mutation_repair_mode && phase_guard.last_validate_failed {
            let post_state = crate::data_engineer::progress_controller::ExecutionState::load_strict(
                &thread_store,
                thread_id,
            )
            .await?
            .unwrap_or_else(crate::data_engineer::progress_controller::ExecutionState::new);
            let snapshot = crate::data_engineer::progress_controller::snapshot_authoring_stepboundary_progress(
                hard_mutation_repair_mode,
                phase_guard.last_validate_failed,
                pre_mutation_epoch,
                post_state.mutation_epoch,
                post_state.stall_count,
                post_state.max_stall_count,
            );
            match crate::data_engineer::authoring_driver::AuthoringDriver::run_turn(
                &authoring_ctx,
                &snapshot,
            ) {
                crate::data_engineer::authoring_driver::AuthoringTurnResult::HardError {
                    message,
                } => return Err(message),
                crate::data_engineer::authoring_driver::AuthoringTurnResult::Continue => {}
            }
        }
        // Deterministic single-step handoff: return to outer controller loop.
        return Ok(PhaseExecutorOutcome::Continue);
    }
    Err(e) => return Err(e),
}
                
    }
}
