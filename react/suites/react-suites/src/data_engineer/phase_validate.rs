use super::*;
use crate::data_engineer::control_flow::Phase;

impl DataEngineerSuite {
    pub(super) async fn execute_validate_phase(
        thread_store: &ThreadStore,
        thread_id: &str,
        phase: crate::data_engineer::control_flow::Phase,
        _question: &str,
        sctx: &SuiteCtx,
        _execution_state: &crate::data_engineer::progress_controller::ExecutionState,
        _guard: &crate::data_engineer::control_flow::DerivedGuardState,
        _allow_ask_approval: bool,
        _thread_state_step_count: usize,
        _last_validate_brief: &Option<String>,
        _last_validate_failed_models: &[crate::data_engineer::progress_controller::FailedModelRef],
    ) -> Result<PhaseExecutorOutcome, String> {

let actx = Self::agent_tool_ctx(thread_id, sctx);
{
    let mut es =
        crate::data_engineer::progress_controller::ExecutionState::load(
            &thread_store,
            thread_id,
        )
        .await
        .unwrap_or_else(
            crate::data_engineer::progress_controller::ExecutionState::new,
        );
    let tier = if phase == Phase::CleanseValidate {
        crate::data_engineer::progress_controller::ExecutionTier::Cleanse
    } else {
        crate::data_engineer::progress_controller::ExecutionTier::Model
    };
    es.enter_validate_mode(tier);
    es.save(&thread_store, thread_id).await.map_err(|e| {
        format!(
            "failed to persist execution state before validate: {e}"
        )
    })?;
}
let emit_trace = |ctx: &react_core::agent::AgentCtx, line: &str| {
    if let Some(tx) = ctx.trace_tx.as_ref() {
        let _ = tx.send(line.to_string());
    }
};

// Pre-validate normalization: dedupe/merge repeated model+test definitions to
// avoid deterministic compile loops before dbt_validate.
if let Err(e) =
    crate::data_engineer::schema_policy::normalize_schema_artifacts_for_validate(
        &actx,
    )
    .await
{
    let reason = format!(
        "Pre-validation normalization failed; fix DBT YAML artifacts before re-validating.\n\n{e}"
    );
    apply_guard_block(
        &thread_store,
        thread_id,
        phase,
        GuardBlockKind::PrecheckFailed,
        reason.clone(),
    )
    .await?;
    let to_phase = if phase == Phase::CleanseValidate {
        Phase::CleanseAuthor
    } else {
        Phase::ModelAuthor
    };
    let _ = apply_phase_transition(
        &thread_store,
        thread_id,
        Some(phase),
        to_phase,
        control_flow::TransitionIntent::Loopback,
        Some(PhaseReasonCode::PrecheckFailed),
        Some(serde_json::json!({ "error": e })),
    )
    .await?;
    return Ok(PhaseExecutorOutcome::Continue);
}

// Cheap structural prechecks: fail fast on malformed/duplicated schema artifacts
// instead of burning a full dbt_validate cycle.
if let Err(e) =
    crate::data_engineer::schema_policy::prevalidate_dbt_schema_artifacts(&actx)
        .await
{
    let reason = format!("Pre-validation failed; fix DBT YAML artifacts before re-validating.\n\n{e}");
    apply_guard_block(
        &thread_store,
        thread_id,
        phase,
        GuardBlockKind::PrecheckFailed,
        reason.clone(),
    )
    .await?;
    let to_phase = if phase == Phase::CleanseValidate {
        Phase::CleanseAuthor
    } else {
        Phase::ModelAuthor
    };
    let _ = apply_phase_transition(
        &thread_store,
        thread_id,
        Some(phase),
        to_phase,
        control_flow::TransitionIntent::Loopback,
        Some(PhaseReasonCode::PrecheckFailed),
        Some(serde_json::json!({ "error": e })),
    )
    .await?;
    return Ok(PhaseExecutorOutcome::Continue);
}

// Targeted pre-check (compile selected, then build selected) based on most recent patch.
// If it fails, we skip full validation and proceed with the standard failure handling.
let obs: crate::data_engineer::controller_event::ValidateObservationContract;
// Hard cutover: do not derive control-state selectors from thread logs.
// Targeted validate remains disabled until selectors are sourced from typed state artifacts.
let select_terms: Vec<String> = Vec::new();
if !select_terms.is_empty() {
    emit_trace(&actx, "targeted compile started");
    let args_compile = serde_json::json!({"build": false, "run": false, "select": select_terms.clone(), "targeted": true, "targeted_step": "compile"});
    let tool_id_compile = uuid::Uuid::new_v4().to_string();
    let _ = thread_store
        .append_step(
            thread_id,
            react_core::session::ThreadStep::ToolStart {
                tool_id: tool_id_compile.clone(),
                name: "dbt_validate".to_string(),
                clean_name: "Validate DBT (compile)".to_string(),
                args: args_compile.clone(),
                status: ToolStepStatus::Running,
                payload: None,
                ctx: None,
                ts: chrono::Utc::now().to_rfc3339(),
                agent: "agent".to_string(),
            },
        )
        .await;
    let obs_compile = control_flow::DeterministicDbtValidateTargetedOnce::run(
        &actx,
        &select_terms,
        false,
        false,
    )
    .await?;
    let obs_compile_norm = react_core::session::ToolObservation::normalize(
        obs_compile.observation.clone(),
    );
    let _ = thread_store
        .append_step(
            thread_id,
            react_core::session::ThreadStep::ToolEnd {
                tool_id: tool_id_compile,
                name: "dbt_validate".to_string(),
                clean_name: "Validate DBT (compile)".to_string(),
                args: args_compile,
                status: if obs_compile_norm.ok {
                    ToolStepStatus::Ok
                } else {
                    ToolStepStatus::Failed
                },
                payload: None,
                ctx: None,
                observation: obs_compile_norm,
                ts: chrono::Utc::now().to_rfc3339(),
                agent: "agent".to_string(),
            },
        )
        .await;
    let ok = obs_compile.outcome_v2.ok;
    let compile_ok = obs_compile.outcome_v2.compile_ok;
    if !(ok && compile_ok) {
        emit_trace(&actx, "targeted compile failed");
        obs = obs_compile;
    } else {
        emit_trace(&actx, "targeted compile ok");
        emit_trace(&actx, "targeted build started");
        let args_build = serde_json::json!({"build": true, "run": false, "select": select_terms.clone(), "targeted": true, "targeted_step": "build"});
        let tool_id_build = uuid::Uuid::new_v4().to_string();
        let _ = thread_store
            .append_step(
                thread_id,
                react_core::session::ThreadStep::ToolStart {
                    tool_id: tool_id_build.clone(),
                    name: "dbt_validate".to_string(),
                    clean_name: "Validate DBT (build)".to_string(),
                    args: args_build.clone(),
                    status: ToolStepStatus::Running,
                    payload: None,
                    ctx: None,
                    ts: chrono::Utc::now().to_rfc3339(),
                    agent: "agent".to_string(),
                },
            )
            .await;
        let obs_build =
            control_flow::DeterministicDbtValidateTargetedOnce::run(
                &actx,
                &select_terms,
                true,
                false,
            )
            .await?;
        let obs_build_norm = react_core::session::ToolObservation::normalize(
            obs_build.observation.clone(),
        );
        let _ = thread_store
            .append_step(
                thread_id,
                react_core::session::ThreadStep::ToolEnd {
                    tool_id: tool_id_build,
                    name: "dbt_validate".to_string(),
                    clean_name: "Validate DBT (build)".to_string(),
                    args: args_build,
                    status: if obs_build_norm.ok {
                        ToolStepStatus::Ok
                    } else {
                        ToolStepStatus::Failed
                    },
                    payload: None,
                    ctx: None,
                    observation: obs_build_norm,
                    ts: chrono::Utc::now().to_rfc3339(),
                    agent: "agent".to_string(),
                },
            )
            .await;
        let ok = obs_build.outcome_v2.ok;
        let compile_ok = obs_build.outcome_v2.compile_ok;
        let run_ok = obs_build.outcome_v2.run_ok;
        if !(ok && compile_ok && run_ok) {
            emit_trace(&actx, "targeted build failed");
            obs = obs_build;
        } else {
            emit_trace(&actx, "targeted build ok");
            // Deterministic full validate (NO repair loop / no mutation).
            let args_full = serde_json::json!({"build": true});
            let tool_id_full = uuid::Uuid::new_v4().to_string();
            let _ = thread_store
                .append_step(
                    thread_id,
                    react_core::session::ThreadStep::ToolStart {
                        tool_id: tool_id_full.clone(),
                        name: "dbt_validate".to_string(),
                        clean_name: "Validate DBT".to_string(),
                        args: args_full.clone(),
                        status: ToolStepStatus::Running,
                        payload: None,
                        ctx: None,
                        ts: chrono::Utc::now().to_rfc3339(),
                        agent: "agent".to_string(),
                    },
                )
                .await;
            obs = control_flow::DeterministicDbtValidateOnce::run(
                &actx, true, false, None,
            )
            .await?;
            let obs_norm = react_core::session::ToolObservation::normalize(
                obs.observation.clone(),
            );
            let _ = thread_store
                .append_step(
                    thread_id,
                    react_core::session::ThreadStep::ToolEnd {
                        tool_id: tool_id_full,
                        name: "dbt_validate".to_string(),
                        clean_name: "Validate DBT".to_string(),
                        args: args_full,
                        status: if obs_norm.ok {
                            ToolStepStatus::Ok
                        } else {
                            ToolStepStatus::Failed
                        },
                        payload: None,
                        ctx: None,
                        observation: obs_norm,
                        ts: chrono::Utc::now().to_rfc3339(),
                        agent: "agent".to_string(),
                    },
                )
                .await;
        }
    }
} else {
    // Deterministic full validate (NO repair loop / no mutation).
    let args_full = serde_json::json!({"build": true});
    let tool_id_full = uuid::Uuid::new_v4().to_string();
    let _ = thread_store
        .append_step(
            thread_id,
            react_core::session::ThreadStep::ToolStart {
                tool_id: tool_id_full.clone(),
                name: "dbt_validate".to_string(),
                clean_name: "Validate DBT".to_string(),
                args: args_full.clone(),
                status: ToolStepStatus::Running,
                payload: None,
                ctx: None,
                ts: chrono::Utc::now().to_rfc3339(),
                agent: "agent".to_string(),
            },
        )
        .await;
    obs = control_flow::DeterministicDbtValidateOnce::run(
        &actx, true, false, None,
    )
    .await?;
    let obs_norm = react_core::session::ToolObservation::normalize(
        obs.observation.clone(),
    );
    let _ = thread_store
        .append_step(
            thread_id,
            react_core::session::ThreadStep::ToolEnd {
                tool_id: tool_id_full,
                name: "dbt_validate".to_string(),
                clean_name: "Validate DBT".to_string(),
                args: args_full,
                status: if obs_norm.ok {
                    ToolStepStatus::Ok
                } else {
                    ToolStepStatus::Failed
                },
                payload: None,
                ctx: None,
                observation: obs_norm,
                ts: chrono::Utc::now().to_rfc3339(),
                agent: "agent".to_string(),
            },
        )
        .await;
}

let validate_event =
    crate::data_engineer::controller_event::validate_event_from_contract(&obs);
if matches!(
    validate_event,
    crate::data_engineer::controller_event::ControllerEvent::ValidatePassed
) {
    // Update canonical execution state (hard-cutover: primary decision source).
    {
        let tier = if phase == Phase::CleanseValidate {
            crate::data_engineer::progress_controller::ExecutionTier::Cleanse
        } else {
            crate::data_engineer::progress_controller::ExecutionTier::Model
        };
        crate::data_engineer::state_manager::apply_execution_event(
            &thread_store,
            thread_id,
            crate::data_engineer::progress_controller::DataEngineerEvent::ValidatePassed {
                tier,
            },
        )
        .await
        .map_err(|e| {
            format!("failed to persist execution state after validate pass: {e}")
        })?;
    }

    // Mark the active plan completed only when validate passes AND checklist execution is complete.
    let mut completion_snapshot: Option<
        crate::data_engineer::plan::PlanCompletionSnapshot,
    > = None;
    let mut active_plan_key: Option<String> = None;
    if phase == Phase::CleanseValidate {
        if let Some(mut p) =
            crate::data_engineer::plan::load_cleanse_plan_any(&actx).await
        {
            crate::data_engineer::plan::apply_cleanse_progress_event(
                &mut p,
                crate::data_engineer::plan::PlanProgressEvent::CleanseValidateDone,
            );
            let snap = crate::data_engineer::plan::snapshot_cleanse_completion(&p);
            active_plan_key = Some(p.plan_key.clone());
            if snap.all_done {
                p.status = crate::data_engineer::plan::PlanStatus::Completed;
            } else {
                p.status = crate::data_engineer::plan::PlanStatus::Approved;
            }
            completion_snapshot = Some(snap);
            let _ =
                crate::data_engineer::plan::save_cleanse_plan(&actx, &p).await;
        }
    } else {
        if let Some(mut p) =
            crate::data_engineer::plan::load_model_plan_any(&actx).await
        {
            crate::data_engineer::plan::apply_model_progress_event(
                &mut p,
                crate::data_engineer::plan::PlanProgressEvent::ModelValidateDone,
            );
            let snap = crate::data_engineer::plan::snapshot_model_completion(&p);
            active_plan_key = Some(p.plan_key.clone());
            if snap.all_done {
                p.status = crate::data_engineer::plan::PlanStatus::Completed;
            } else {
                p.status = crate::data_engineer::plan::PlanStatus::Approved;
            }
            completion_snapshot = Some(snap);
            let _ =
                crate::data_engineer::plan::save_model_plan(&actx, &p).await;
        }
    }

    if completion_snapshot
        .as_ref()
        .map(|snap| !snap.all_done)
        .unwrap_or(false)
    {
        let signal = "ValidatePassPlanIncomplete";
        let reason = format!(
            "{}: dbt_validate succeeded but plan checklist work is still pending; returning to authoring",
            signal
        );
        apply_guard_block(
            &thread_store,
            thread_id,
            phase,
            GuardBlockKind::AuthoringCompletion,
            reason.clone(),
        )
        .await?;
        let to_phase = if phase == Phase::CleanseValidate {
            Phase::CleanseAuthor
        } else {
            Phase::ModelAuthor
        };
        apply_phase_transition(
            &thread_store,
            thread_id,
            Some(phase),
            to_phase,
            control_flow::TransitionIntent::Loopback,
            Some(PhaseReasonCode::ValidatePassToAuthoring),
            Some(crate::data_engineer::phase_reason_detail::to_value(
                &crate::data_engineer::phase_reason_detail::ValidatePassToAuthoringDetail {
                    signal: signal.to_string(),
                    plan_key: active_plan_key,
                    pending_count: completion_snapshot
                        .as_ref()
                        .map(|snap| snap.pending_count)
                        .unwrap_or(0),
                    pending_refs: completion_snapshot
                        .as_ref()
                        .map(|snap| snap.pending_refs.clone())
                        .map(|v| serde_json::to_value(v).unwrap_or(serde_json::Value::Null))
                        .unwrap_or(serde_json::Value::Array(vec![])),
                    dbt_validate_observation: obs.observation.clone(),
                    next_action: "resume_authoring_for_remaining_plan_work".to_string(),
                    audit_acceptance: Self::churn_audit_acceptance_criteria(),
                },
            )),
        )
        .await?;
        return Ok(PhaseExecutorOutcome::Continue);
    }

    let trigger_step_idx = thread_store
        .get(thread_id)
        .await
        .map(|l| l.steps.len().saturating_sub(1))
        .unwrap_or(0);
    let to_phase = if phase == Phase::CleanseValidate {
        Phase::CleanseReview
    } else {
        Phase::ModelReview
    };
    apply_phase_transition(
        &thread_store,
        thread_id,
        Some(phase),
        to_phase,
        control_flow::TransitionIntent::Forward,
        Some(PhaseReasonCode::ValidatePassToReview),
        Some(crate::data_engineer::phase_reason_detail::to_value(
            &crate::data_engineer::phase_reason_detail::ValidatePassToReviewDetail {
                dbt_validate_observation: obs.observation.clone(),
                dbt_validate_step_idx: trigger_step_idx,
            },
        )),
    )
    .await?;
    return Ok(PhaseExecutorOutcome::Continue);
}

// Warehouse config failures require user action.
let (
    failure_class,
    failure_signature,
    brief,
    failing_targets,
    compile_ok,
    run_ok,
) = match validate_event {
    crate::data_engineer::controller_event::ControllerEvent::ValidateFailed {
        class,
        signature,
        brief,
        failing_targets,
        compile_ok,
        run_ok,
    } => (class, signature, brief, failing_targets, compile_ok, run_ok),
    crate::data_engineer::controller_event::ControllerEvent::ValidateContractError {
        reason,
        brief,
    } => {
        return Err(format!(
            "validate_outcome_v2_contract_error: {} ({})",
            reason, brief
        ));
    }
    crate::data_engineer::controller_event::ControllerEvent::ValidatePassed => {
        // Covered by the success branch above.
        unreachable!("validate pass should have continued above")
    }
};
if matches!(
    failure_class,
    crate::data_engineer::controller_event::ValidateFailureClass::WarehouseConfig
) {
    return Err(format!(
        "dbt_validate failed due to a warehouse/aws configuration issue: {}",
        brief
    ));
}
let errs: Vec<String> = obs
    .observation
    .get("errors")
    .and_then(|v| serde_json::from_value(v.clone()).ok())
    .unwrap_or_default();

// Update canonical execution state from this validate failure (hard-cutover: primary decision source).
{
    let tier = if phase == Phase::CleanseValidate {
        crate::data_engineer::progress_controller::ExecutionTier::Cleanse
    } else {
        crate::data_engineer::progress_controller::ExecutionTier::Model
    };
    let failing_models: Vec<
        crate::data_engineer::progress_controller::FailedModelRef,
    > = failing_targets
        .iter()
        .map(|t| crate::data_engineer::progress_controller::FailedModelRef {
            name: t.node_id.clone(),
            file: t.canonical_path.clone(),
        })
        .collect();
    let backlog = crate::data_engineer::progress_controller::repair_backlog_from_failed_models(&failing_models);
    let failure_class_state = match failure_class {
        crate::data_engineer::controller_event::ValidateFailureClass::WarehouseConfig => {
            crate::data_engineer::progress_controller::FailureClass::WarehouseConfig
        }
        crate::data_engineer::controller_event::ValidateFailureClass::SqlOrRuntime => {
            crate::data_engineer::progress_controller::FailureClass::SqlOrRuntime
        }
        crate::data_engineer::controller_event::ValidateFailureClass::Unknown => {
            crate::data_engineer::progress_controller::FailureClass::Unknown
        }
    };
    crate::data_engineer::state_manager::apply_execution_event(
        &thread_store,
        thread_id,
        crate::data_engineer::progress_controller::DataEngineerEvent::ValidateFailed {
            tier,
            failure_class: failure_class_state,
            failure_signature: crate::data_engineer::progress_controller::FailureSignature {
                class: failure_class_state,
                node_id: Some(failure_signature.node_id.clone()),
                canonical_path: Some(failure_signature.canonical_path.clone()),
                error_code: Some(failure_signature.error_code.clone()),
            },
            backlog,
            brief: Some(brief.clone()),
            compile_ok: Some(compile_ok),
            run_ok: Some(run_ok),
        },
    )
    .await
    .map_err(|e| {
        format!("failed to persist execution state after validate failure: {e}")
    })?;
}

// Validation failed -> go back to corresponding author phase.
let trigger_step_idx = thread_store
    .get(thread_id)
    .await
    .map(|l| l.steps.len().saturating_sub(1))
    .unwrap_or(0);
let to_phase = if phase == Phase::CleanseValidate {
    Phase::CleanseAuthor
} else {
    Phase::ModelAuthor
};
// Attach authoritative schema facts for the next authoring turn. This ensures the LLM
// never needs to guess relation columns after a deterministic validate failure.
let dialect = obs
    .observation
    .get("dialect")
    .and_then(|v| v.as_str())
    .unwrap_or("Unknown SQL dialect")
    .to_string();
let facts_bundle = crate::data_engineer::facts::build_validate_fail_facts(
    &actx,
    dialect,
    &obs,
    crate::data_engineer::facts::FactsScope::ValidateFail,
    crate::data_engineer::facts::FactsLimits::for_scope(
        crate::data_engineer::facts::FactsScope::ValidateFail,
    ),
)
.await;
// Best-effort persist into the active plan snapshot for reuse in subsequent authoring turns.
// Keep bounded to avoid unbounded plan growth.
if phase == Phase::CleanseValidate {
    if let Some(mut p) =
        crate::data_engineer::plan::load_cleanse_plan(&actx).await
    {
        if p.project_snapshot.is_null() {
            p.project_snapshot = serde_json::json!({});
        }
        if let Some(obj) = p.project_snapshot.as_object_mut() {
            let arr = obj
                .entry("validate_fail_facts")
                .or_insert_with(|| serde_json::Value::Array(vec![]));
            if let Some(a) = arr.as_array_mut() {
                a.push(
                    serde_json::to_value(&facts_bundle)
                        .unwrap_or(serde_json::Value::Null),
                );
                while a.len() > 5 {
                    a.remove(0);
                }
            }
        }
        crate::data_engineer::plan::save_cleanse_plan(&actx, &p)
            .await
            .map_err(|e| {
                format!(
                    "failed to persist cleanse validate-failure facts bundle: {e}"
                )
            })?;
    }
} else {
    if let Some(mut p) =
        crate::data_engineer::plan::load_model_plan(&actx).await
    {
        if p.project_snapshot.is_null() {
            p.project_snapshot = serde_json::json!({});
        }
        if let Some(obj) = p.project_snapshot.as_object_mut() {
            let arr = obj
                .entry("validate_fail_facts")
                .or_insert_with(|| serde_json::Value::Array(vec![]));
            if let Some(a) = arr.as_array_mut() {
                a.push(
                    serde_json::to_value(&facts_bundle)
                        .unwrap_or(serde_json::Value::Null),
                );
                while a.len() > 5 {
                    a.remove(0);
                }
            }
        }
        crate::data_engineer::plan::save_model_plan(&actx, &p)
            .await
            .map_err(|e| {
                format!(
                    "failed to persist model validate-failure facts bundle: {e}"
                )
            })?;
    }
}
apply_phase_transition(
    &thread_store,
    thread_id,
    Some(phase),
    to_phase,
    control_flow::TransitionIntent::Loopback,
    Some(PhaseReasonCode::ValidateFail),
    Some(crate::data_engineer::phase_reason_detail::to_value(
        &crate::data_engineer::phase_reason_detail::ValidateFailDetail {
            dbt_validate_observation: obs.observation.clone(),
            dbt_validate_step_idx: trigger_step_idx,
            errors: errs,
            facts_bundle: serde_json::to_value(&facts_bundle).unwrap_or(serde_json::Value::Null),
        },
    )),
)
.await?;
return Ok(PhaseExecutorOutcome::Continue);
                
    }
}
