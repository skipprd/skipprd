use crate::domain_types::{ReviewDecision, ReviewDecisionMeta, ReviewTier};
use crate::phase_contract::{commit_phase_decision, PhaseDecision};
use crate::progress_controller::PhaseTransition;
use crate::review_batched;
use crate::{control_flow, DataEngineerSuite, PhaseError, PhaseOutcome};
use react_core::session::ThreadStore;
use react_core::suite::{FlowFrame, FlowKind, SuiteCtx};

fn effective_review_tier(phase: control_flow::Phase, tier: ReviewTier) -> ReviewTier {
    if tier != ReviewTier::Unknown {
        return tier;
    }
    match phase {
        control_flow::Phase::CleanseReview => ReviewTier::Silver,
        control_flow::Phase::ModelReview => ReviewTier::Gold,
        _ => ReviewTier::Unknown,
    }
}

fn patch_impl_target_phase(phase: control_flow::Phase, tier: ReviewTier) -> control_flow::Phase {
    match effective_review_tier(phase, tier) {
        ReviewTier::Silver => control_flow::Phase::CleanseAuthor,
        ReviewTier::Gold | ReviewTier::Unknown => control_flow::Phase::ModelAuthor,
    }
}

impl DataEngineerSuite {
    pub(super) async fn execute_review_phase(
        thread_store: &ThreadStore,
        thread_id: &str,
        phase: control_flow::Phase,
        question: &str,
        sctx: &SuiteCtx,
        execution_state: &crate::progress_controller::ExecutionState,
        thread_state_step_count: usize,
        out_frames: &mut Vec<FlowFrame>,
    ) -> Result<PhaseOutcome, PhaseError> {
        let review_q = Self::build_review_question_with_context(question, phase, execution_state);
        let frames = match review_batched::run_batched_review(thread_id, &review_q, phase, sctx)
            .await
        {
            Ok(f) => f,
            Err(e) => {
                let validate_passed = !execution_state.last_validate_failed();
                if validate_passed {
                    tracing::warn!(
                        "data_engineer: review infra failure (phase={}) but validate already passed; proceeding. error={}",
                        phase.as_str(),
                        e
                    );
                    let next = match phase {
                        control_flow::Phase::CleanseReview => control_flow::Phase::ModelPlan,
                        control_flow::Phase::ModelReview => {
                            control_flow::Phase::PublishAwaitApproval
                        }
                        _ => control_flow::Phase::Done,
                    };
                    out_frames.push(FlowFrame::Review {
                        text: format!(
                            "Review skipped due to infrastructure error (validate passed): {}",
                            e
                        ),
                        meta: None,
                    });
                    commit_phase_decision(
                        thread_store,
                        thread_id,
                        Some(phase),
                        PhaseDecision::forward(next, Some(PhaseTransition::ReviewProceed)),
                    )
                    .await?;
                    return Ok(PhaseOutcome::TransitionCommitted);
                }
                return Err(PhaseError::from(e));
            }
        };
        let first = frames.into_iter().next().unwrap_or(FlowFrame::Complete {
            kind: FlowKind::new("generic"),
            payload: serde_json::json!({ "text": "" }),
            display: None,
        });
        let (answer, decision_meta_v) = match first {
            FlowFrame::Complete {
                payload, display, ..
            } => {
                let ans = display
                    .or_else(|| {
                        payload
                            .get("text")
                            .and_then(|x| x.as_str())
                            .map(|s| s.to_string())
                    })
                    .unwrap_or_default();
                let mv = payload.get("meta").cloned();
                (ans, mv)
            }
            other => return Ok(PhaseOutcome::Return(vec![other])),
        };

        let trigger_step_idx = thread_state_step_count.saturating_sub(1);
        let trigger_step = serde_json::json!({
            "phase": phase.as_str(),
            "phase_reason_code": execution_state
                .phase
                .transition
                .as_ref()
                .map(|t| t.as_reason_str().to_string()),
            "phase_reason_detail": execution_state.phase.transition.as_ref().and_then(|t| serde_json::to_value(t).ok()),
        });

        let mut meta: ReviewDecisionMeta = match decision_meta_v {
            Some(v) => serde_json::from_value(v).map_err(|e| {
                PhaseError::ToolContractViolation(format!("review meta did not match schema: {e}"))
            })?,
            None => {
                return Err(PhaseError::ToolContractViolation(
                    "review meta missing from review result".to_string(),
                ))
            }
        };
        let review_ref_from_trigger = trigger_step
            .get("phase_reason_detail")
            .cloned()
            .filter(|v| !v.is_null())
            .and_then(|v| {
                serde_json::from_value::<
                        crate::phase_reason_detail::ReviewDecisionTransitionDetail,
                    >(v)
                    .ok()
            })
            .and_then(|detail| detail.meta.review_ref);
        if meta.review_ref.is_none() {
            meta.review_ref = review_ref_from_trigger;
        }
        let review_retry_count;
        let mut forced_proceed_by_patch_exhaustion = false;
        let subjective_kind = match meta.decision {
            ReviewDecision::PatchImpl => {
                Some(crate::progress_controller::SubjectiveRetryKind::ReviewPatchImpl)
            }
            ReviewDecision::PlanChange => {
                Some(crate::progress_controller::SubjectiveRetryKind::ReviewPlanChange)
            }
            _ => None,
        };
        if let Some(kind) = subjective_kind {
            use crate::retry_budget::SubjectiveRetryOutcome;
            match Self::check_subjective_retry_budget(thread_store, thread_id, kind).await? {
                SubjectiveRetryOutcome::Exhausted(tries) => {
                    tracing::warn!(
                        "data_engineer: review {:?} retry budget exhausted; forcing proceed phase={} tries={}",
                        kind,
                        phase.as_str(),
                        tries
                    );
                    Self::clear_subjective_retries(
                        thread_store,
                        thread_id,
                        vec![
                            crate::progress_controller::SubjectiveRetryKind::ReviewPatchImpl,
                            crate::progress_controller::SubjectiveRetryKind::ReviewPlanChange,
                        ],
                    )
                    .await?;
                    review_retry_count = tries;
                    meta.decision = ReviewDecision::Proceed;
                    forced_proceed_by_patch_exhaustion = true;
                }
                SubjectiveRetryOutcome::WithinBudget(tries) => {
                    review_retry_count = tries;
                }
            }
        } else {
            review_retry_count = 0;
            Self::clear_subjective_retries(
                thread_store,
                thread_id,
                vec![
                    crate::progress_controller::SubjectiveRetryKind::ReviewPatchImpl,
                    crate::progress_controller::SubjectiveRetryKind::ReviewPlanChange,
                ],
            )
            .await?;
        }
        out_frames.push(FlowFrame::Review {
            text: answer.clone(),
            meta: Some(
                serde_json::to_value(&meta).map_err(|e| {
                    PhaseError::Fatal(format!("failed to serialize review meta: {e}"))
                })?,
            ),
        });

        let _reason_detail = crate::phase_reason_detail::review_decision_transition(
            phase.as_str(),
            meta.clone(),
            meta.decision,
            forced_proceed_by_patch_exhaustion,
            false,
            review_retry_count,
            trigger_step_idx,
            trigger_step,
        );

        match meta.decision {
            ReviewDecision::Proceed => {
                crate::state_manager::apply_execution_event(
                    &thread_store.control_store(),
                    thread_id,
                    crate::progress_controller::DataEngineerEvent::PatchImplIntentCleared,
                )
                .await
                .map(|_| ())?;
                let next = match phase {
                    control_flow::Phase::CleanseReview => control_flow::Phase::ModelPlan,
                    control_flow::Phase::ModelReview => control_flow::Phase::PublishAwaitApproval,
                    _ => control_flow::Phase::Done,
                };
                if next == control_flow::Phase::PublishAwaitApproval {
                    crate::state_manager::apply_execution_event(
                        &thread_store.control_store(),
                        thread_id,
                        crate::progress_controller::DataEngineerEvent::PublishApprovalSet {
                            decision: crate::progress_controller::PublishApprovalDecision::Approved,
                        },
                    )
                    .await
                    .map_err(|e| {
                        format!(
                            "failed to persist explicit publish approval on model review proceed: {e}"
                        )
                    })?;
                }
                commit_phase_decision(
                    thread_store,
                    thread_id,
                    Some(phase),
                    PhaseDecision::forward(next, Some(PhaseTransition::ReviewProceed)),
                )
                .await?;
                Ok(PhaseOutcome::TransitionCommitted)
            }
            ReviewDecision::PatchImpl => {
                let back = patch_impl_target_phase(phase, meta.tier);
                let review_brief = {
                    let cleaned = Self::strip_meta_line(&answer);
                    let max = 4_000usize;
                    if cleaned.len() > max {
                        format!("Review feedback (truncated):\n{}...", &cleaned[..max])
                    } else {
                        format!("Review feedback:\n{cleaned}")
                    }
                };
                crate::state_manager::apply_execution_event(
                    &thread_store.control_store(),
                    thread_id,
                    crate::progress_controller::DataEngineerEvent::ValidateCheckFailed {
                        failure_context: crate::progress_controller::ValidationFailureContext {
                            brief: review_brief,
                            log_excerpts: None,
                            compile_ok: true,
                            run_ok: true,
                        },
                    },
                )
                .await
                .map_err(|e| format!("failed to record review feedback as failure context: {e}"))?;
                crate::state_manager::apply_execution_event(
                    &thread_store.control_store(),
                    thread_id,
                    crate::progress_controller::DataEngineerEvent::PatchImplIntentSet {
                        phase: back,
                    },
                )
                .await
                .map(|_| ())?;
                commit_phase_decision(
                    thread_store,
                    thread_id,
                    Some(phase),
                    PhaseDecision::loopback(
                        back,
                        Some(PhaseTransition::ReviewPatchImpl {
                            meta: meta.clone(),
                            target_paths: meta.dataset_ids.clone(),
                        }),
                    ),
                )
                .await?;
                Ok(PhaseOutcome::TransitionCommitted)
            }
            ReviewDecision::PlanChange => {
                crate::state_manager::apply_execution_event(
                    &thread_store.control_store(),
                    thread_id,
                    crate::progress_controller::DataEngineerEvent::PatchImplIntentCleared,
                )
                .await
                .map(|_| ())?;
                let cleaned = DataEngineerSuite::strip_meta_line(&answer);
                let evidence = {
                    let trimmed = cleaned.trim();
                    let max = 1_000usize;
                    if trimmed.len() > max {
                        format!("{}...", &trimmed[..max])
                    } else {
                        trimmed.to_string()
                    }
                };
                let violation = crate::progress_controller::PlanViolation::new(
                    phase,
                    None,
                    format!("Review requested plan change: {evidence}"),
                );
                crate::phase_contract::commit_plan_revision_loopback(
                    thread_store,
                    thread_id,
                    phase,
                    vec![violation],
                    crate::progress_controller::PlanRevisionStrategy::Rewrite,
                )
                .await?;
                Ok(PhaseOutcome::TransitionCommitted)
            }
        }
    }
}
