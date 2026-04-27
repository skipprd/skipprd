use crate::phase_contract::{commit_metered_decision, commit_phase_decision, PhaseDecision};
use crate::progress_controller::{DataEngineerEvent, PhaseTransition, PublishRetryKind};
use crate::{control_flow, state_manager, tools, DataEngineerSuite, PhaseError, PhaseOutcome};
use react_core::session::ThreadStore;
use react_core::suite::{FlowFrame, FlowKind, SuiteCtx};

async fn check_publish_retry_limit(
    thread_store: &ThreadStore,
    thread_id: &str,
    kind: PublishRetryKind,
    error_prefix: &str,
) -> Result<(usize, Option<Result<PhaseOutcome, PhaseError>>), PhaseError> {
    let retry_limit = crate::controller_kernel::publish_retry_limit();
    let es = state_manager::apply_execution_event(
        &thread_store.control_store(),
        thread_id,
        DataEngineerEvent::PublishRetryBumped {
            kind,
            cap: retry_limit,
        },
    )
    .await
    .map_err(|e| PhaseError::Fatal(format!("failed to persist publish retry state: {e}")))?;
    let retry_count = es.publish_status().retry_count_for_kind(kind);
    let err = if retry_count > retry_limit {
        Some(Err(format!("{error_prefix}: retries={retry_count}").into()))
    } else {
        None
    };
    Ok((retry_count, err))
}

impl DataEngineerSuite {
    pub(super) async fn execute_publish_await_approval_phase(
        thread_store: &ThreadStore,
        thread_id: &str,
        _sctx: &SuiteCtx,
    ) -> Result<PhaseOutcome, PhaseError> {
        let es = crate::progress_controller::ExecutionState::load_strict(
            &thread_store.control_store(),
            thread_id,
        )
        .await?
        .unwrap_or_else(crate::progress_controller::ExecutionState::new);
        if let Err(reason) = crate::progress_controller::gate_publish_progress(
            &es,
            control_flow::Phase::PublishAwaitApproval,
        ) {
            let (_retry_count, err) = check_publish_retry_limit(
                thread_store,
                thread_id,
                PublishRetryKind::AwaitApprovalLoop,
                &format!("publish_await_approval_not_converged_after_retries; reason={reason}"),
            )
            .await?;
            if let Some(err) = err {
                return err;
            }
            return Ok(PhaseOutcome::stayed_waiting(format!(
                "publish await-approval gate remains unsatisfied: {reason}"
            )));
        }

        commit_phase_decision(
            thread_store,
            thread_id,
            Some(control_flow::Phase::PublishAwaitApproval),
            PhaseDecision::forward(
                control_flow::Phase::Publish,
                Some(PhaseTransition::PublishApproved),
            ),
        )
        .await?;
        Ok(PhaseOutcome::TransitionCommitted)
    }

    pub(super) async fn execute_publish_phase(
        thread_store: &ThreadStore,
        thread_id: &str,
        sctx: &SuiteCtx,
    ) -> Result<PhaseOutcome, PhaseError> {
        let es = crate::progress_controller::ExecutionState::load_strict(
            &thread_store.control_store(),
            thread_id,
        )
        .await?
        .unwrap_or_else(crate::progress_controller::ExecutionState::new);
        if let Err(reason) =
            crate::progress_controller::gate_publish_progress(&es, control_flow::Phase::Publish)
        {
            return Err(reason.into());
        }
        let actx = Self::agent_tool_ctx(thread_id, sctx);
        let tool = tools::publish_dbt_to_provider::PublishDbtToProviderTool {
            datasets: crate::ctx_ext::sctx_datasets(sctx),
            catalog: crate::ctx_ext::sctx_catalog(sctx),
        };
        let obs = control_flow::call_and_record_tool(
            thread_store,
            thread_id,
            Some(crate::env_util::DEFAULT_AGENT_NAME.to_string()),
            &tool,
            serde_json::json!({"confirm": true}),
            &actx,
            600,
        )
        .await;
        let ok = obs.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        let stage = obs.get("stage").and_then(|v| v.as_str()).unwrap_or("");
        if ok && (stage == "published" || stage == "no_change") {
            state_manager::apply_execution_event(
                &thread_store.control_store(),
                thread_id,
                DataEngineerEvent::PublishRetriesReset,
            )
            .await
            .map_err(|e| {
                PhaseError::Fatal(format!(
                    "failed to persist publish confirmed-success state: {e}"
                ))
            })?;
            let published_count = crate::plan::load_model_plan(&actx)
                .await
                .ok()
                .flatten()
                .map(|p| p.tasks.len() as u64)
                .unwrap_or(1);
            commit_metered_decision(
                thread_store,
                thread_id,
                Some(control_flow::Phase::Publish),
                PhaseDecision::forward(
                    control_flow::Phase::Done,
                    Some(PhaseTransition::PublishConfirmedSuccess),
                ),
                vec![
                    crate::metering::UsageEvent::ModelsPublished {
                        count: published_count,
                        project_id: thread_id.to_string(),
                    },
                    crate::metering::UsageEvent::PipelineCompleted {
                        project_id: thread_id.to_string(),
                    },
                ],
                crate::metering::global_metering(),
            )
            .await?;
            return Ok(PhaseOutcome::TransitionCommitted);
        }
        if ok && stage == "await_approval" {
            let (retry_count, err) = check_publish_retry_limit(
                thread_store,
                thread_id,
                PublishRetryKind::AwaitApprovalLoop,
                "publish_await_approval_not_converged_after_retries",
            )
            .await?;
            if let Some(err) = err {
                state_manager::apply_execution_event(
                    &thread_store.control_store(),
                    thread_id,
                    DataEngineerEvent::PublishApprovalCleared,
                )
                .await
                .map_err(|e| {
                    PhaseError::Fatal(format!(
                        "failed to persist publish approval fallback state: {e}"
                    ))
                })?;
                return err;
            }
            state_manager::apply_execution_event(
                &thread_store.control_store(),
                thread_id,
                DataEngineerEvent::PublishApprovalCleared,
            )
            .await
            .map_err(|e| {
                PhaseError::Fatal(format!(
                    "failed to persist publish approval fallback state: {e}"
                ))
            })?;
            commit_phase_decision(
                thread_store,
                thread_id,
                Some(control_flow::Phase::Publish),
                PhaseDecision::loopback(
                    control_flow::Phase::PublishAwaitApproval,
                    Some(PhaseTransition::PublishFail { retry_count }),
                ),
            )
            .await?;
            return Ok(PhaseOutcome::TransitionCommitted);
        }
        let (_retry_count, err) = check_publish_retry_limit(
            thread_store,
            thread_id,
            PublishRetryKind::PublishFailureLoop,
            "publish_confirmed_failure_not_converged_after_retries",
        )
        .await?;
        if let Some(err) = err {
            state_manager::apply_execution_event(
                &thread_store.control_store(),
                thread_id,
                DataEngineerEvent::PublishApprovalCleared,
            )
            .await
            .map_err(|e| {
                PhaseError::Fatal(format!(
                    "failed to persist publish confirmed-failure state: {e}"
                ))
            })?;
            return err;
        }
        state_manager::apply_execution_event(
            &thread_store.control_store(),
            thread_id,
            DataEngineerEvent::PublishApprovalCleared,
        )
        .await
        .map_err(|e| {
            PhaseError::Fatal(format!(
                "failed to persist publish confirmed-failure state: {e}"
            ))
        })?;
        commit_phase_decision(
            thread_store,
            thread_id,
            Some(control_flow::Phase::Publish),
            PhaseDecision::loopback(
                control_flow::Phase::ModelAuthor,
                Some(PhaseTransition::PublishConfirmedFail),
            ),
        )
        .await?;
        Ok(PhaseOutcome::TransitionCommitted)
    }

    pub(super) fn execute_done_phase(
        out_frames: &mut Vec<FlowFrame>,
    ) -> Result<PhaseOutcome, PhaseError> {
        let mut answer = "Agent flow completed (deterministic phases): cleanse → validate → review → model → validate → review → publish.\n".to_string();
        if let Some(last) = out_frames.iter().rev().find_map(|f| match f {
            FlowFrame::Review { text, .. } => Some(text.clone()),
            _ => None,
        }) {
            answer.push_str("\nLatest review summary:\n");
            answer.push_str(&last);
        }
        out_frames.push(FlowFrame::Complete {
            kind: FlowKind::new("generic"),
            payload: serde_json::json!({ "text": answer.clone() }),
            display: Some(answer),
        });
        Ok(PhaseOutcome::Return(out_frames.clone()))
    }
}
