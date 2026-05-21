use crate::domain_types::{ReviewDecision, ReviewDecisionMeta};
use crate::phase_contract::{commit_phase_decision, PhaseDecision};
use crate::progress_controller::PhaseTransition;
use crate::review_batched;
use crate::{control_flow, DataEngineerSuite, PhaseError, PhaseOutcome};
use react_core::session::ThreadStore;
use react_core::suite::{FlowFrame, FlowKind, SuiteCtx};

fn model_review_needs_plan_change_targets(
    phase: control_flow::Phase,
    meta: &ReviewDecisionMeta,
) -> bool {
    phase == control_flow::Phase::ModelReview
        && meta.decision == ReviewDecision::PlanChange
        && meta.target_task_ids.is_empty()
}

async fn persist_publish_approval_for_review_proceed(
    thread_store: &ThreadStore,
    thread_id: &str,
) -> Result<(), PhaseError> {
    crate::state_manager::apply_execution_event(
        &thread_store.control_store(),
        thread_id,
        crate::progress_controller::DataEngineerEvent::PublishApprovalSet {
            decision: crate::progress_controller::PublishApprovalDecision::Approved,
        },
    )
    .await
    .map_err(|e| {
        PhaseError::Fatal(format!(
            "failed to persist explicit publish approval on model review proceed: {e}"
        ))
    })?;
    Ok(())
}

fn model_plan_change_target_retry_question(review_q: &str, prior_answer: &str) -> String {
    format!(
        "{review_q}\n\nMODEL PLAN CHANGE TARGET RETRY:\n\
Your previous review requested decision=\"plan_change\" but returned target_task_ids=[].\n\
For model review, target_task_ids means affected plan task IDs exactly as shown in review batch_items.\n\
Prefer surgical plan changes wherever possible. If only specific model plan tasks need changes, return decision=\"plan_change\" with target_task_ids set to those exact task IDs.\n\
Do not use DBT file paths, warehouse relation FQNs, or source catalog dataset IDs as target_task_ids.\n\
Leave target_task_ids empty only if the approved model plan truly needs a complete rewrite.\n\n\
Previous review answer:\n{}\n",
        DataEngineerSuite::excerpt(prior_answer, 4_000)
    )
}

fn review_answer_and_meta(first: FlowFrame) -> Result<(String, ReviewDecisionMeta), PhaseError> {
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
        _other => {
            return Err(PhaseError::Fatal(format!(
                "review returned unexpected frame instead of completion"
            )))
        }
    };
    let meta: ReviewDecisionMeta = match decision_meta_v {
        Some(v) => serde_json::from_value(v).map_err(|e| {
            PhaseError::ToolContractViolation(format!("review meta did not match schema: {e}"))
        })?,
        None => {
            return Err(PhaseError::ToolContractViolation(
                "review meta missing from review result".to_string(),
            ))
        }
    };
    Ok((answer, meta))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain_types::ReviewTier;
    use react_core::keyspace::DefaultKeyspace;
    use react_core::scope::RequestScope;
    use react_module_storage_memory::InMemoryStorageAdapter;
    use std::sync::Arc;

    fn meta(target_task_ids: Vec<&str>) -> ReviewDecisionMeta {
        ReviewDecisionMeta {
            decision: ReviewDecision::PlanChange,
            tier: ReviewTier::Gold,
            target_task_ids: target_task_ids.into_iter().map(str::to_string).collect(),
            review_ref: None,
        }
    }

    #[test]
    fn unscoped_model_plan_change_requests_target_retry() {
        assert!(model_review_needs_plan_change_targets(
            control_flow::Phase::ModelReview,
            &meta(vec![])
        ));
        assert!(!model_review_needs_plan_change_targets(
            control_flow::Phase::ModelReview,
            &meta(vec!["target_task"])
        ));
        assert!(!model_review_needs_plan_change_targets(
            control_flow::Phase::CleanseReview,
            &meta(vec![])
        ));
    }

    #[test]
    fn model_plan_change_target_retry_prompt_requires_surgical_ids() {
        let prompt = model_plan_change_target_retry_question("review goal", "prior answer");
        assert!(prompt.contains("review batch_items"));
        assert!(prompt.contains("Do not use DBT file paths"));
        assert!(prompt.contains("target_task_ids"));
        assert!(prompt.contains("Leave target_task_ids empty only if"));
        assert!(prompt.contains("complete rewrite"));
        assert!(prompt.contains("prior answer"));
    }

    #[tokio::test]
    async fn model_review_proceed_publish_approval_satisfies_publish_gate() {
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let scope = RequestScope::parse("t", "w", "p").expect("valid test scope");
        let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
        let store = ThreadStore::new(storage, scope, keyspace);
        let tid = "tid-model-review-proceed-publish-approval";

        persist_publish_approval_for_review_proceed(&store, tid)
            .await
            .expect("persist publish approval");
        let state =
            crate::progress_controller::ExecutionState::load_strict(&store.control_store(), tid)
                .await
                .expect("load state")
                .expect("state exists");

        assert!(crate::progress_controller::gate_publish_progress(
            &state,
            control_flow::Phase::PublishAwaitApproval
        )
        .is_ok());
        assert!(crate::progress_controller::gate_publish_progress(
            &state,
            control_flow::Phase::Publish
        )
        .is_ok());
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
                    if next == control_flow::Phase::PublishAwaitApproval {
                        persist_publish_approval_for_review_proceed(thread_store, thread_id)
                            .await?;
                    }
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
        let (mut answer, mut meta) = match first {
            FlowFrame::Complete {
                payload,
                display,
                kind,
            } => review_answer_and_meta(FlowFrame::Complete {
                kind,
                payload,
                display,
            })?,
            other => return Ok(PhaseOutcome::Return(vec![other])),
        };
        if model_review_needs_plan_change_targets(phase, &meta) {
            tracing::warn!(
                "data_engineer: model review requested plan change without model targets; retrying once for surgical target ids"
            );
            let retry_q = model_plan_change_target_retry_question(&review_q, &answer);
            match review_batched::run_batched_review(thread_id, &retry_q, phase, sctx).await {
                Ok(retry_frames) => {
                    let retry_first =
                        retry_frames
                            .into_iter()
                            .next()
                            .unwrap_or(FlowFrame::Complete {
                                kind: FlowKind::new("generic"),
                                payload: serde_json::json!({ "text": "" }),
                                display: None,
                            });
                    match retry_first {
                        FlowFrame::Complete {
                            payload,
                            display,
                            kind,
                        } => {
                            let (retry_answer, retry_meta) =
                                review_answer_and_meta(FlowFrame::Complete {
                                    kind,
                                    payload,
                                    display,
                                })?;
                            answer = retry_answer;
                            meta = retry_meta;
                            if model_review_needs_plan_change_targets(phase, &meta) {
                                tracing::warn!(
                                    "data_engineer: model review target retry still omitted model ids; accepting as full plan rewrite request"
                                );
                            }
                        }
                        other => return Ok(PhaseOutcome::Return(vec![other])),
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "data_engineer: model review target retry failed; keeping original unscoped plan change. error={}",
                        e
                    );
                }
            }
        }

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
        let review_retry_count = 0;
        let forced_proceed_by_patch_exhaustion = false;
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
                    persist_publish_approval_for_review_proceed(thread_store, thread_id).await?;
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
                let review_brief = {
                    let cleaned = Self::strip_meta_line(&answer);
                    let max = 4_000usize;
                    if cleaned.len() > max {
                        format!("Review feedback (truncated):\n{}...", &cleaned[..max])
                    } else {
                        format!("Review feedback:\n{cleaned}")
                    }
                };
                let evidence =
                    crate::evaluation::Evidence::Review(crate::evaluation::ReviewEvidence {
                        brief: review_brief,
                        target_task_ids: meta.target_task_ids.clone(),
                        requests_plan_change: false,
                    });
                let actx = Self::agent_tool_ctx(thread_id, sctx);
                let input =
                    crate::evaluation::build_input(&actx, phase, evidence.clone(), execution_state)
                        .await?;
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
            ReviewDecision::PlanChange => {
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
                let evidence =
                    crate::evaluation::Evidence::Review(crate::evaluation::ReviewEvidence {
                        brief: evidence,
                        target_task_ids: meta.target_task_ids.clone(),
                        requests_plan_change: true,
                    });
                let actx = Self::agent_tool_ctx(thread_id, sctx);
                let input =
                    crate::evaluation::build_input(&actx, phase, evidence.clone(), execution_state)
                        .await?;
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
    }
}
