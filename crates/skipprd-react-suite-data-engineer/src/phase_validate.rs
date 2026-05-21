use super::*;
use crate::control_flow::Phase;

const MAX_VALIDATE_FAIL_FACTS: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ValidatePassTransition {
    ToAuthoring,
    ToReview,
}

impl DataEngineerSuite {
    fn model_validate_failure_plan_binding(
        plan: &crate::plan::ModelPlan,
    ) -> crate::facts::ValidateFailurePlanBinding {
        let task_spec_digests = plan
            .tasks
            .iter()
            .filter_map(|task| {
                crate::authoring_contract::model_task_spec_digest(&plan.plan_key, task)
                    .map(|digest| (task.name.clone(), digest))
            })
            .collect();
        crate::facts::ValidateFailurePlanBinding {
            plan_key: Some(plan.plan_key.clone()),
            task_spec_digests,
        }
    }

    fn precheck_failure_to_evidence(
        precheck: crate::schema_policy::PrecheckFailure,
    ) -> crate::evaluation::Evidence {
        let crate::schema_policy::PrecheckFailure {
            kind,
            brief,
            suggested_targets,
        } = precheck;
        let kind_label = match kind {
            crate::schema_policy::PrecheckFailureKind::SchemaModelMisplacement => {
                "schema_model_misplacement"
            }
            crate::schema_policy::PrecheckFailureKind::DuplicateModelDefinition => {
                "duplicate_model_definition"
            }
            crate::schema_policy::PrecheckFailureKind::DuplicateSqlModelStem => {
                "duplicate_sql_model_stem"
            }
            crate::schema_policy::PrecheckFailureKind::StagingSchemaColumnMismatch => {
                "staging_schema_column_mismatch"
            }
            crate::schema_policy::PrecheckFailureKind::InvalidTestDefinitionShape => {
                "invalid_test_definition_shape"
            }
            crate::schema_policy::PrecheckFailureKind::Other => "other",
        };
        let log_excerpts = if suggested_targets.is_empty() {
            Some(format!(
                "precheck_failure_kind: {kind_label}\nsuggested_targets: (none — investigate via file op=list)"
            ))
        } else {
            Some(format!(
                "precheck_failure_kind: {kind_label}\nsuggested_targets:\n- {}",
                suggested_targets.join("\n- ")
            ))
        };
        crate::evaluation::Evidence::SchemaPrecheck(crate::evaluation::PrecheckEvidence {
            brief,
            log_excerpts,
            suggested_targets,
        })
    }

    async fn build_evaluation_input(
        actx: &AgentCtx,
        phase: Phase,
        evidence: crate::evaluation::Evidence,
        execution_state: &crate::progress_controller::ExecutionState,
    ) -> Result<crate::evaluation::EvaluationInput, PhaseError> {
        crate::evaluation::build_input(actx, phase, evidence, execution_state)
            .await
            .map_err(Into::into)
    }

    pub(super) async fn apply_evaluation_verdict(
        thread_store: &ThreadStore,
        thread_id: &str,
        phase: Phase,
        sctx: &SuiteCtx,
        execution_state: &crate::progress_controller::ExecutionState,
        evidence: crate::evaluation::Evidence,
        verdict: crate::evaluation::EvaluationVerdict,
    ) -> Result<PhaseOutcome, PhaseError> {
        let evidence_hash = evidence.hash();
        let key = crate::evaluation::AttemptKey {
            phase: phase.as_str().to_string(),
            verdict_kind: verdict.kind(),
            evidence_hash: evidence_hash.clone(),
        };
        let next_attempt = execution_state
            .attempt_ledger()
            .count(&key)
            .saturating_add(1);

        crate::state_manager::apply_execution_event(
            &thread_store.control_store(),
            thread_id,
            crate::progress_controller::DataEngineerEvent::AttemptRecorded { key },
        )
        .await
        .map_err(|e| format!("failed to record evaluation attempt: {e}"))?;
        crate::state_manager::apply_execution_event(
            &thread_store.control_store(),
            thread_id,
            crate::progress_controller::DataEngineerEvent::EvaluationVerdictRecorded {
                verdict: verdict.summary(),
                evidence_hash: evidence_hash.clone(),
            },
        )
        .await
        .map_err(|e| format!("failed to record evaluation verdict: {e}"))?;

        if next_attempt > crate::evaluation::DEFAULT_ATTEMPT_CAP {
            return Err(PhaseError::Fatal(format!(
                "evaluation verdict {:?} repeated {} times for the same evidence; manual intervention required.\n\n{}",
                verdict.kind(),
                next_attempt,
                evidence.brief()
            )));
        }

        match verdict {
            crate::evaluation::EvaluationVerdict::Accepted => Ok(PhaseOutcome::Return(vec![])),
            crate::evaluation::EvaluationVerdict::Fatal { reason } => {
                Err(PhaseError::Fatal(reason))
            }
            crate::evaluation::EvaluationVerdict::RefreshTruth { reason: _ } => {
                crate::phase_contract::commit_phase_decision(
                    thread_store,
                    thread_id,
                    Some(phase),
                    crate::phase_contract::PhaseDecision::loopback(
                        phase,
                        Some(crate::progress_controller::PhaseTransition::PhaseBlocked),
                    ),
                )
                .await?;
                Ok(PhaseOutcome::TransitionCommitted)
            }
            crate::evaluation::EvaluationVerdict::RevisePlan {
                violations,
                strategy,
            } => {
                crate::phase_contract::commit_plan_revision_loopback(
                    thread_store,
                    thread_id,
                    phase,
                    violations,
                    strategy,
                )
                .await?;
                Ok(PhaseOutcome::TransitionCommitted)
            }
            crate::evaluation::EvaluationVerdict::RepairImplementation { evidence } => {
                let actx = Self::agent_tool_ctx(thread_id, sctx);
                let cfg = crate::resolved_config_from_ctx(&actx);
                let dispatch = cfg
                    .map(|c| crate::model_dispatch::ModelDispatch::from_resolved(&c.llm))
                    .unwrap_or_else(|| crate::model_dispatch::ModelDispatch {
                        reason_model: "gpt-4o-mini".into(),
                        task_model: "gpt-4o-mini".into(),
                    });
                let repair_result = crate::repair_subroutine::run_repair(
                    sctx,
                    thread_store,
                    thread_id,
                    &dispatch,
                    evidence.repair_context(),
                    None,
                    next_attempt,
                )
                .await;
                match repair_result {
                    Ok(_) => {
                        crate::phase_contract::commit_metered_decision(
                            thread_store,
                            thread_id,
                            Some(phase),
                            crate::phase_contract::PhaseDecision::loopback(
                                phase,
                                Some(crate::progress_controller::PhaseTransition::RepairCompleted),
                            ),
                            vec![crate::metering::UsageEvent::RepairCycle {
                                cycle: next_attempt as u64,
                                project_id: thread_id.to_string(),
                            }],
                            crate::metering::global_metering(),
                        )
                        .await?;
                        Ok(PhaseOutcome::TransitionCommitted)
                    }
                    Err(e) => Err(PhaseError::Fatal(format!("repair failed: {e}"))),
                }
            }
        }
    }
    fn decide_validate_pass_transition(
        completion_snapshot: Option<&crate::plan::PlanCompletionSnapshot>,
    ) -> ValidatePassTransition {
        match completion_snapshot
            .map(|snap| snap.completion_state())
            .unwrap_or(crate::plan::PlanCompletionState::Complete)
        {
            crate::plan::PlanCompletionState::Incomplete => ValidatePassTransition::ToAuthoring,
            crate::plan::PlanCompletionState::Complete => ValidatePassTransition::ToReview,
        }
    }

    async fn reduce_validate_pass_plan_state(
        actx: &AgentCtx,
        phase: Phase,
    ) -> Result<
        (
            Option<crate::plan::PlanCompletionSnapshot>,
            Option<String>,
            u64,
        ),
        String,
    > {
        let mut completion_snapshot: Option<crate::plan::PlanCompletionSnapshot> = None;
        let mut active_plan_key: Option<String> = None;
        let mut validated_count: u64 = 0;
        if phase == Phase::CleanseValidate {
            if let Some(mut p) = crate::plan::load_cleanse_plan(actx)
                .await
                .map_err(|e| e.to_string())?
            {
                validated_count = p.tasks.len() as u64;
                crate::plan::apply_cleanse_progress_event(
                    &mut p,
                    crate::plan::PlanProgressEvent::CleanseValidateDone,
                );
                let snap = crate::plan::snapshot_cleanse_completion(&p);
                active_plan_key = Some(p.plan_key.clone());
                if snap.all_done {
                    p.status = crate::plan::PlanStatus::Completed;
                } else {
                    p.status = crate::plan::PlanStatus::Approved;
                }
                completion_snapshot = Some(snap);
                crate::plan::save_cleanse_plan(actx, &p)
                    .await
                    .map_err(|e| {
                        format!("failed to persist cleanse plan validate-pass state: {e}")
                    })?;
            }
        } else if let Some(mut p) = crate::plan::load_model_plan(actx)
            .await
            .map_err(|e| e.to_string())?
        {
            validated_count = p.tasks.len() as u64;
            crate::plan::apply_model_progress_event(
                &mut p,
                crate::plan::PlanProgressEvent::ModelValidateDone,
            );
            let snap = crate::plan::snapshot_model_completion(&p);
            active_plan_key = Some(p.plan_key.clone());
            if snap.all_done {
                p.status = crate::plan::PlanStatus::Completed;
            } else {
                p.status = crate::plan::PlanStatus::Approved;
            }
            completion_snapshot = Some(snap);
            crate::plan::save_model_plan(actx, &p)
                .await
                .map_err(|e| format!("failed to persist model plan validate-pass state: {e}"))?;
        }
        Ok((completion_snapshot, active_plan_key, validated_count))
    }

    async fn commit_validate_pass_transition(
        thread_store: &ThreadStore,
        thread_id: &str,
        phase: Phase,
        thread_state_step_count: usize,
        completion_snapshot: Option<crate::plan::PlanCompletionSnapshot>,
        validated_count: u64,
        active_plan_key: Option<String>,
        dbt_validate_observation: serde_json::Value,
        signal: &str,
    ) -> Result<(), String> {
        if Self::decide_validate_pass_transition(completion_snapshot.as_ref())
            == ValidatePassTransition::ToAuthoring
        {
            let reason = format!(
                "{}: dbt_validate succeeded but plan checklist work is still pending; returning to authoring",
                signal
            );
            let _detail = crate::phase_reason_detail::to_value(
                &crate::phase_reason_detail::ValidatePassToAuthoringDetail {
                    signal: signal.to_string(),
                    plan_key: active_plan_key.clone(),
                    pending_count: completion_snapshot
                        .as_ref()
                        .map(|snap| snap.pending_count)
                        .unwrap_or(0),
                    pending_refs: completion_snapshot
                        .as_ref()
                        .map(|snap| snap.pending_refs.clone())
                        .map(|v| serde_json::to_value(v).unwrap_or(serde_json::Value::Null))
                        .unwrap_or(serde_json::Value::Array(vec![])),
                    dbt_validate_observation,
                    next_action: crate::phase_reason_detail::ValidateToAuthoringNextAction::ResumeAuthoringForRemainingPlanWork,
                    audit_acceptance: Self::churn_audit_acceptance_criteria(),
                },
            );
            crate::retry_budget::guard_block_loopback_to_author(
                thread_store,
                thread_id,
                phase,
                GuardBlockKind::AuthoringCompletion,
                reason,
                crate::progress_controller::PhaseTransition::ValidatePassToAuthoring {
                    signal: signal.to_string(),
                    plan_key: active_plan_key.clone(),
                    pending_count: completion_snapshot
                        .as_ref()
                        .map(|snap| snap.pending_count)
                        .unwrap_or(0),
                },
            )
            .await?;
            return Ok(());
        }

        let to_phase = if phase == Phase::CleanseValidate {
            Phase::CleanseReview
        } else {
            Phase::ModelReview
        };
        let trigger_step_idx = thread_state_step_count.saturating_sub(1);
        crate::phase_contract::commit_metered_decision(
            thread_store,
            thread_id,
            Some(phase),
            crate::phase_contract::PhaseDecision::forward(
                to_phase,
                Some(
                    crate::progress_controller::PhaseTransition::ValidatePassToReview {
                        step_idx: trigger_step_idx,
                    },
                ),
            ),
            vec![crate::metering::UsageEvent::ModelsValidated {
                count: validated_count,
                project_id: thread_id.to_string(),
            }],
            crate::metering::global_metering(),
        )
        .await?;
        Ok(())
    }

    pub(super) async fn execute_validate_phase(
        thread_store: &ThreadStore,
        thread_id: &str,
        phase: crate::control_flow::Phase,
        _question: &str,
        sctx: &SuiteCtx,
        execution_state: &crate::progress_controller::ExecutionState,
        thread_state_step_count: usize,
    ) -> Result<PhaseOutcome, PhaseError> {
        let actx = Self::agent_tool_ctx(thread_id, sctx);
        // Pre-validate normalization: dedupe/merge repeated model+test definitions to
        // avoid deterministic compile loops before dbt_validate.
        if let Err(e) = crate::schema_policy::normalize_schema_artifacts_for_validate(&actx).await {
            // Normalization failures share the same routing policy as schema prechecks: send
            // the agent into the repair loop with a structured "Other" failure rather than
            // consuming the deprecated precheck-specific retry budget.
            let precheck = crate::schema_policy::PrecheckFailure::other(format!(
                "Pre-validation normalization failed; fix DBT YAML artifacts before re-validating.\n\n{e}"
            ));
            let evidence = Self::precheck_failure_to_evidence(precheck);
            let input =
                Self::build_evaluation_input(&actx, phase, evidence.clone(), execution_state)
                    .await?;
            let verdict = crate::evaluation::evaluate(input);
            return Self::apply_evaluation_verdict(
                thread_store,
                thread_id,
                phase,
                sctx,
                execution_state,
                evidence,
                verdict,
            )
            .await;
        }

        // Cheap structural prechecks: fail fast on malformed/duplicated schema artifacts
        // instead of burning a full dbt_validate cycle.
        //
        // Precheck failures route to the repair subroutine, not the author loopback, because
        // they describe structural relocations the LLM should solve in one focused
        // gather→reason→apply cycle. We deliberately do NOT consume the `ValidatePrecheckFailed`
        // subjective retry budget here — the repair loop owns its own retry budget and an
        // overly aggressive precheck budget was previously causing premature Fatal escalations.
        // The structured [`PrecheckFailure`] becomes evaluation evidence so the repair prompt gets
        // the failure kind and the list of files to look at first.
        if let Err(precheck_failure) =
            crate::schema_policy::prevalidate_dbt_schema_artifacts(&actx).await
        {
            let evidence = Self::precheck_failure_to_evidence(precheck_failure);
            let input =
                Self::build_evaluation_input(&actx, phase, evidence.clone(), execution_state)
                    .await?;
            let verdict = crate::evaluation::evaluate(input);
            return Self::apply_evaluation_verdict(
                thread_store,
                thread_id,
                phase,
                sctx,
                execution_state,
                evidence,
                verdict,
            )
            .await;
        }

        // Deterministic full validate (NO repair loop / no mutation).
        let args_full = serde_json::json!({"build": true});
        let mut validate_ctx = react_core::session::ExecutionContext::default();
        validate_ctx.set(
            "phase",
            serde_json::Value::String(phase.as_str().to_string()),
        );
        validate_ctx.set(
            "tier",
            serde_json::Value::String(
                if phase == Phase::CleanseValidate {
                    "cleanse"
                } else {
                    "model"
                }
                .to_string(),
            ),
        );
        validate_ctx.set(
            "validate_mode",
            serde_json::Value::String("deterministic_full_build".to_string()),
        );
        validate_ctx.set("build", serde_json::Value::Bool(true));
        if let Err(failure) = crate::runtime_prereqs::ensure_dbt_runtime_prerequisites(&actx).await
        {
            let evidence = failure.into_evidence();
            let input =
                Self::build_evaluation_input(&actx, phase, evidence.clone(), execution_state)
                    .await?;
            let verdict = crate::evaluation::evaluate(input);
            return Self::apply_evaluation_verdict(
                thread_store,
                thread_id,
                phase,
                sctx,
                execution_state,
                evidence,
                verdict,
            )
            .await;
        }
        let obs = {
            let meta = react_core::session::ToolStepMeta {
                agent: crate::env_util::DEFAULT_AGENT_NAME.to_string(),
                phase: phase.as_str().to_string(),
                name: "dbt_validate".to_string(),
                clean_name: "Validate DBT".to_string(),
                args: args_full,
                ctx: Some(validate_ctx),
            };
            match thread_store
                .run_observed(
                    thread_id,
                    meta,
                    || async {
                        control_flow::DeterministicDbtValidateOnce::run(&actx, true, false, None)
                            .await
                    },
                    |contract| Ok(contract.observation.clone()),
                )
                .await
            {
                Ok(contract) => contract,
                Err(e) => {
                    tracing::warn!(
                        "data_engineer: validate execution failed (thread_id={} phase={}): {}",
                        thread_id,
                        phase.as_str(),
                        e
                    );
                    let evidence = crate::evaluation::Evidence::DbtValidateExecution {
                        error: format!("dbt_validate execution failed: {e}"),
                    };
                    let input = Self::build_evaluation_input(
                        &actx,
                        phase,
                        evidence.clone(),
                        execution_state,
                    )
                    .await?;
                    let verdict = crate::evaluation::evaluate(input);
                    return Self::apply_evaluation_verdict(
                        thread_store,
                        thread_id,
                        phase,
                        sctx,
                        execution_state,
                        evidence,
                        verdict,
                    )
                    .await;
                }
            }
        };

        let validate_event = crate::controller_event::validate_event_from_contract(&obs);
        if matches!(
            validate_event,
            crate::controller_event::ControllerEvent::ValidatePassed
        ) {
            // Update canonical execution state (hard-cutover: primary decision source).
            {
                let tier = if phase == Phase::CleanseValidate {
                    crate::progress_controller::ExecutionTier::Cleanse
                } else {
                    crate::progress_controller::ExecutionTier::Model
                };
                crate::state_manager::apply_execution_event(
                    &thread_store.control_store(),
                    thread_id,
                    crate::progress_controller::DataEngineerEvent::ValidatePassed { tier },
                )
                .await
                .map_err(|e| {
                    format!("failed to persist execution state after validate pass: {e}")
                })?;
            }

            if phase == Phase::ModelValidate {
                crate::phase_plan_lifecycle::refresh_model_plan_grounded_schemas(sctx, &actx).await;
            }

            let (completion_snapshot, active_plan_key, validated_count) =
                Self::reduce_validate_pass_plan_state(&actx, phase).await?;

            Self::commit_validate_pass_transition(
                &thread_store,
                thread_id,
                phase,
                thread_state_step_count,
                completion_snapshot,
                validated_count,
                active_plan_key,
                obs.observation.clone(),
                "ValidatePassPlanIncomplete",
            )
            .await?;
            return Ok(PhaseOutcome::TransitionCommitted);
        }

        let (brief, failure_hash, compile_ok, run_ok) = match validate_event {
            crate::controller_event::ControllerEvent::ValidateFailed {
                brief,
                failure_hash,
                compile_ok,
                run_ok,
            } => (brief, failure_hash, compile_ok, run_ok),
            crate::controller_event::ControllerEvent::ValidateContractError { reason, brief } => {
                tracing::warn!(
                    "data_engineer: validate contract error: {} ({}) (thread_id={} phase={})",
                    reason,
                    brief,
                    thread_id,
                    phase.as_str()
                );
                let err_reason =
                    format!("validate_outcome_v2_contract_error: {} ({})", reason, brief);
                let evidence =
                    crate::evaluation::Evidence::DbtValidateExecution { error: err_reason };
                let input =
                    Self::build_evaluation_input(&actx, phase, evidence.clone(), execution_state)
                        .await?;
                let verdict = crate::evaluation::evaluate(input);
                return Self::apply_evaluation_verdict(
                    thread_store,
                    thread_id,
                    phase,
                    sctx,
                    execution_state,
                    evidence,
                    verdict,
                )
                .await;
            }
            crate::controller_event::ControllerEvent::ValidatePassed => {
                unreachable!("validate pass should have continued above")
            }
        };
        let errs: Vec<String> = obs
            .observation
            .get("errors")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        if let Some(failure) = crate::runtime_prereqs::failure_from_dbt_errors(&brief, &errs) {
            let evidence = failure.into_evidence();
            let input =
                Self::build_evaluation_input(&actx, phase, evidence.clone(), execution_state)
                    .await?;
            let verdict = crate::evaluation::evaluate(input);
            return Self::apply_evaluation_verdict(
                thread_store,
                thread_id,
                phase,
                sctx,
                execution_state,
                evidence,
                verdict,
            )
            .await;
        }
        let log_excerpts = {
            let raw = crate::dbt_error::extract_log_excerpts(&obs.observation, 3, 6000);
            if raw.trim().is_empty() {
                None
            } else {
                Some(raw)
            }
        };
        // Attach authoritative schema facts for the next authoring turn. This ensures the LLM
        // never needs to guess relation columns after a deterministic validate failure.
        let dialect = crate::facts::SqlDialect(
            obs.observation
                .get("dialect")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown SQL dialect")
                .to_string(),
        );
        let plan_binding = if phase == Phase::ModelValidate {
            crate::plan::load_model_plan(&actx)
                .await?
                .as_ref()
                .map(Self::model_validate_failure_plan_binding)
        } else {
            None
        };
        let facts_bundle = crate::facts::build_validate_fail_facts(
            &actx,
            dialect,
            &obs,
            crate::facts::FactsScope::ValidateFail,
            plan_binding,
        )
        .await;
        // Best-effort persist into the active plan snapshot for reuse in subsequent authoring turns.
        // Keep bounded to avoid unbounded plan growth.
        if phase == Phase::CleanseValidate {
            if let Some(mut p) = crate::plan::load_cleanse_plan(&actx).await? {
                p.project_snapshot
                    .validate_fail_facts
                    .push(facts_bundle.clone());
                if p.project_snapshot.validate_fail_facts.len() > MAX_VALIDATE_FAIL_FACTS {
                    p.project_snapshot.validate_fail_facts.remove(0);
                }
                crate::plan::save_cleanse_plan(&actx, &p)
                    .await
                    .map_err(|e| {
                        format!("failed to persist cleanse validate-failure facts bundle: {e}")
                    })?;
            }
        } else {
            if let Some(mut p) = crate::plan::load_model_plan(&actx).await? {
                p.project_snapshot
                    .validate_fail_facts
                    .push(facts_bundle.clone());
                if p.project_snapshot.validate_fail_facts.len() > MAX_VALIDATE_FAIL_FACTS {
                    p.project_snapshot.validate_fail_facts.remove(0);
                }
                crate::plan::save_model_plan(&actx, &p).await.map_err(|e| {
                    format!("failed to persist model validate-failure facts bundle: {e}")
                })?;
            }
        }
        let evidence =
            crate::evaluation::Evidence::DbtValidate(crate::evaluation::DbtValidateEvidence {
                brief,
                failure_hash,
                compile_ok,
                run_ok,
                log_excerpts,
                errors: errs,
            });
        let input =
            Self::build_evaluation_input(&actx, phase, evidence.clone(), execution_state).await?;
        let verdict = crate::evaluation::evaluate(input);
        Self::apply_evaluation_verdict(
            thread_store,
            thread_id,
            phase,
            sctx,
            execution_state,
            evidence,
            verdict,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_pass_transition_decision_is_typed() {
        let incomplete = crate::plan::PlanCompletionSnapshot {
            all_done: false,
            pending_count: 2,
            pending_refs: vec![],
        };
        let complete = crate::plan::PlanCompletionSnapshot {
            all_done: true,
            pending_count: 0,
            pending_refs: vec![],
        };
        assert_eq!(
            DataEngineerSuite::decide_validate_pass_transition(Some(&incomplete)),
            ValidatePassTransition::ToAuthoring
        );
        assert_eq!(
            DataEngineerSuite::decide_validate_pass_transition(Some(&complete)),
            ValidatePassTransition::ToReview
        );
        assert_eq!(
            DataEngineerSuite::decide_validate_pass_transition(None),
            ValidatePassTransition::ToReview
        );
    }

    #[test]
    fn precheck_failure_becomes_schema_precheck_evidence() {
        let precheck = crate::schema_policy::PrecheckFailure {
            kind: crate::schema_policy::PrecheckFailureKind::SchemaModelMisplacement,
            brief: "models/schema.yml contains staging model 'stg_picnic_screen_birthdate'."
                .to_string(),
            suggested_targets: vec![
                "models/schema.yml".to_string(),
                "models/staging/stg_picnic_screen_birthdate.yml".to_string(),
            ],
        };
        let evidence = DataEngineerSuite::precheck_failure_to_evidence(precheck);
        match evidence {
            crate::evaluation::Evidence::SchemaPrecheck(evidence) => {
                assert!(evidence.brief.contains("stg_picnic_screen_birthdate"));
                let excerpts = evidence.log_excerpts.expect("log_excerpts");
                assert!(excerpts.contains("schema_model_misplacement"));
                assert!(excerpts.contains("models/staging/stg_picnic_screen_birthdate.yml"));
            }
            _ => panic!("expected schema precheck evidence"),
        }
    }
}
