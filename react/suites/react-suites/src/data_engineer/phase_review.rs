use crate::data_engineer::phase_contract::{commit_phase_decision, PhaseDecision};
use crate::data_engineer::review_batched;
use crate::data_engineer::{
    control_flow, stable_json_digest, DataEngineerSuite, PhaseExecutorOutcome,
};
use crate::flow_frame::FlowFrame;
use crate::suite::SuiteCtx;
use react_core::control_flow::{PhaseReasonCode, ReviewDecision, ReviewDecisionMeta, ReviewTier};
use react_core::session::ThreadStore;

fn effective_review_tier(phase: control_flow::Phase, tier: ReviewTier) -> ReviewTier {
    if tier != ReviewTier::Unknown {
        return tier;
    }
    match phase {
        control_flow::Phase::CleanseReview => ReviewTier::Silver,
        control_flow::Phase::ModelReview | control_flow::Phase::PostPublishReview => ReviewTier::Gold,
        _ => ReviewTier::Unknown,
    }
}

fn patch_plan_target_phase(phase: control_flow::Phase, tier: ReviewTier) -> control_flow::Phase {
    match effective_review_tier(phase, tier) {
        ReviewTier::Silver => control_flow::Phase::CleansePlan,
        ReviewTier::Gold | ReviewTier::Unknown => control_flow::Phase::ModelPlan,
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
        execution_state: &crate::data_engineer::progress_controller::ExecutionState,
        out_frames: &mut Vec<FlowFrame>,
    ) -> Result<PhaseExecutorOutcome, String> {
        let review_q = Self::build_review_question_with_context(question, phase, execution_state);
        let frames = review_batched::run_batched_review(thread_id, &review_q, phase, sctx).await?;
        let first = frames.into_iter().next().unwrap_or(FlowFrame::Final {
            kind: "generic".to_string(),
            payload: serde_json::json!({ "text": "" }),
            display: None,
        });
        let (mut answer, decision_meta_v) = match first {
            FlowFrame::Final {
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
            other => return Ok(PhaseExecutorOutcome::Return(vec![other])),
        };

        let (trigger_step_idx, trigger_step) = thread_store
            .get(thread_id)
            .await
            .ok()
            .and_then(|l| {
                let idx = l.steps.len().saturating_sub(1);
                l.steps.last().cloned().map(|s| (idx, s))
            })
            .unwrap_or((
                0,
                react_core::session::ThreadStep::Phase {
                    phase: "unknown".to_string(),
                    from_phase: None,
                    reason_code: Some(PhaseReasonCode::PhaseSet),
                    reason_detail: Some(serde_json::json!({
                        "fallback": "missing_thread_step"
                    })),
                    observation: react_core::session::Observation::ok(),
                    ts: chrono::Utc::now().to_rfc3339(),
                    agent: "agent".to_string(),
                },
            ));

        let mut meta: ReviewDecisionMeta = decision_meta_v
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or(ReviewDecisionMeta {
                decision: ReviewDecision::Proceed,
                tier: ReviewTier::Unknown,
                dataset_ids: vec![],
                review_ref: None,
            });
        let review_ref_from_trigger = match &trigger_step {
            react_core::session::ThreadStep::Phase { reason_detail, .. } => {
                reason_detail
                    .as_ref()
                    .and_then(|v| v.get("review_ref"))
                    .cloned()
            }
            _ => None,
        };
        if meta.review_ref.is_none() {
            meta.review_ref = review_ref_from_trigger;
        }
        let mut review_retry_count = 0usize;
        if let Some(kind) = Self::review_retry_kind(meta.decision) {
            review_retry_count =
                Self::bump_subjective_retry(thread_store, thread_id, phase, kind).await;
        } else {
            Self::reset_subjective_retry(thread_store, thread_id).await;
        }
        let forced_by_subjective_retry = Self::review_retry_kind(meta.decision).is_some()
            && review_retry_count > crate::data_engineer::controller_kernel::subjective_retry_limit();
        let forced_progress = forced_by_subjective_retry;
        if forced_progress {
            let reason = format!(
                "subjective review retry limit reached ({})",
                review_retry_count
            );
            tracing::warn!(
                "data_engineer: forcing review proceed phase={} reason={}",
                phase.as_str(),
                reason
            );
            meta.decision = ReviewDecision::Proceed;
            answer.push_str(&format!(
                "\n\nProgress guard: review remained subjective without convergence (retry_count={}). Proceeding to next phase to avoid non-convergent review loops.",
                review_retry_count,
            ));
            Self::reset_subjective_retry(thread_store, thread_id).await;
        }
        out_frames.push(FlowFrame::Review {
            text: answer.clone(),
            meta: serde_json::to_value(&meta).ok(),
        });

        let reason_detail = crate::data_engineer::phase_reason_detail::review_decision_transition(
            phase.as_str(),
            serde_json::to_value(&meta).unwrap_or(serde_json::Value::Null),
            answer.clone(),
            forced_progress,
            forced_by_subjective_retry,
            review_retry_count,
            trigger_step_idx,
            serde_json::to_value(&trigger_step).unwrap_or(serde_json::Value::Null),
        );

        match meta.decision {
            ReviewDecision::Proceed => {
                crate::data_engineer::loopback_intents::clear_pending_loopback_intent(
                    thread_store,
                    thread_id,
                )
                .await
                .or_else(|e| {
                    if e.contains("not found") {
                        Ok(())
                    } else {
                        Err(format!("failed to clear pending loopback intent: {e}"))
                    }
                })?;
                let next = match phase {
                    control_flow::Phase::CleanseReview => control_flow::Phase::ModelPlan,
                    control_flow::Phase::ModelReview => control_flow::Phase::PublishAwaitApproval,
                    control_flow::Phase::PostPublishReview => control_flow::Phase::Done,
                    _ => control_flow::Phase::Done,
                };
                if next == control_flow::Phase::PublishAwaitApproval {
                    let mut st =
                        crate::data_engineer::progress_controller::ExecutionState::load_strict(
                            thread_store,
                            thread_id,
                        )
                        .await?
                        .unwrap_or_else(
                            crate::data_engineer::progress_controller::ExecutionState::new,
                        );
                    st.set_publish_approval(
                        crate::data_engineer::progress_controller::PublishApprovalDecision::Approved,
                    );
                    st.save(thread_store, thread_id).await.map_err(|e| {
                        format!(
                            "failed to persist explicit publish approval on model review proceed: {e}"
                        )
                    })?;
                }
                commit_phase_decision(
                    thread_store,
                    thread_id,
                    Some(phase),
                    PhaseDecision::forward(
                        next,
                        Some(PhaseReasonCode::ReviewProceed),
                        Some(reason_detail),
                    ),
                )
                .await?;
                Ok(PhaseExecutorOutcome::Continue)
            }
            ReviewDecision::PatchPlan => {
                let back = patch_plan_target_phase(phase, meta.tier);
                let plan_actx = Self::plan_agent_ctx(thread_id, sctx);
                let (entry_plan_key, entry_plan_digest) = match back {
                    control_flow::Phase::CleansePlan => {
                        if let Some(p) = crate::data_engineer::plan::load_cleanse_plan_any(&plan_actx).await {
                            (Some(p.plan_key.clone()), stable_json_digest(&p))
                        } else {
                            (None, None)
                        }
                    }
                    control_flow::Phase::ModelPlan => {
                        if let Some(p) = crate::data_engineer::plan::load_model_plan_any(&plan_actx).await {
                            (Some(p.plan_key.clone()), stable_json_digest(&p))
                        } else {
                            (None, None)
                        }
                    }
                    _ => (None, None),
                };
                let _ = crate::data_engineer::loopback_intents::set_pending_patch_plan_intent(
                    thread_store,
                    thread_id,
                    back,
                    entry_plan_key,
                    entry_plan_digest,
                )
                .await;
                commit_phase_decision(
                    thread_store,
                    thread_id,
                    Some(phase),
                    PhaseDecision::loopback(
                        back,
                        Some(PhaseReasonCode::ReviewPatchPlan),
                        Some(reason_detail),
                    ),
                )
                .await?;
                Ok(PhaseExecutorOutcome::Continue)
            }
            ReviewDecision::PatchImpl => {
                let back = patch_impl_target_phase(phase, meta.tier);
                let _ = crate::data_engineer::loopback_intents::set_pending_patch_impl_intent(
                    thread_store,
                    thread_id,
                    back,
                )
                .await;
                commit_phase_decision(
                    thread_store,
                    thread_id,
                    Some(phase),
                    PhaseDecision::loopback(
                        back,
                        Some(PhaseReasonCode::ReviewPatchImpl),
                        Some(reason_detail),
                    ),
                )
                .await?;
                Ok(PhaseExecutorOutcome::Continue)
            }
        }
    }
}

