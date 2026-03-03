use crate::data_engineer::phase_contract::{commit_phase_decision, PhaseDecision};
use crate::data_engineer::{control_flow, tools, DataEngineerSuite, PhaseExecutorOutcome};
use crate::flow_frame::FlowFrame;
use crate::suite::SuiteCtx;
use react_core::control_flow::PhaseReasonCode;
use react_core::session::ThreadStore;

impl DataEngineerSuite {
    pub(super) async fn execute_publish_await_approval_phase(
        thread_store: &ThreadStore,
        thread_id: &str,
        _sctx: &SuiteCtx,
    ) -> Result<PhaseExecutorOutcome, String> {
        let mut es = crate::data_engineer::progress_controller::ExecutionState::load_strict(
            thread_store,
            thread_id,
        )
        .await?
        .unwrap_or_else(crate::data_engineer::progress_controller::ExecutionState::new);
        if let Err(reason) = crate::data_engineer::progress_controller::gate_publish_progress(
            &es,
            control_flow::Phase::PublishAwaitApproval,
        ) {
            let retry_limit = crate::data_engineer::controller_kernel::publish_retry_limit();
            let retry_count = es.bump_publish_retry(
                crate::data_engineer::progress_controller::PublishRetryKind::AwaitApprovalLoop,
                retry_limit,
            );
            es.save(thread_store, thread_id).await.map_err(|e| {
                format!("failed to persist publish await-approval retry state: {e}")
            })?;
            if retry_count > retry_limit {
                return Err(format!(
                    "publish_await_approval_not_converged_after_retries: retries={retry_count}; reason={reason}"
                ));
            }
            return Ok(PhaseExecutorOutcome::Continue);
        }

        es.reset_publish_retry(
            crate::data_engineer::progress_controller::PublishRetryKind::AwaitApprovalLoop,
        );
        es.save(thread_store, thread_id).await.map_err(|e| {
            format!("failed to persist publish approval consumption state: {e}")
        })?;
        let approval_detail = crate::data_engineer::phase_reason_detail::publish_approval_state(
            serde_json::to_value(&es.publish_approval).unwrap_or(serde_json::Value::Null),
        );
        commit_phase_decision(
            thread_store,
            thread_id,
            Some(control_flow::Phase::PublishAwaitApproval),
            PhaseDecision::forward(
                control_flow::Phase::Publish,
                Some(PhaseReasonCode::UserApprovedPublish),
                Some(crate::data_engineer::phase_reason_detail::publish_auto_approved(
                    serde_json::json!({
                        "approval_state": approval_detail
                    }),
                )),
            ),
        )
        .await?;
        Ok(PhaseExecutorOutcome::Continue)
    }

    pub(super) async fn execute_publish_phase(
        thread_store: &ThreadStore,
        thread_id: &str,
        sctx: &SuiteCtx,
    ) -> Result<PhaseExecutorOutcome, String> {
        let mut es = crate::data_engineer::progress_controller::ExecutionState::load_strict(
            thread_store,
            thread_id,
        )
        .await?
        .unwrap_or_else(crate::data_engineer::progress_controller::ExecutionState::new);
        if let Err(reason) =
            crate::data_engineer::progress_controller::gate_publish_progress(&es, control_flow::Phase::Publish)
        {
            return Err(reason);
        }
        let actx = Self::agent_tool_ctx(thread_id, sctx);
        let tool = tools::publish_dbt_to_provider::PublishDbtToProviderTool {
            datasets: sctx.datasets.clone(),
            catalog: sctx.catalog.clone(),
        };
        let obs = control_flow::call_and_record_tool(
            thread_store,
            thread_id,
            Some("agent".to_string()),
            &tool,
            serde_json::json!({"confirm": true}),
            &actx,
            600,
        )
        .await;
        let ok = obs.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        let stage = obs.get("stage").and_then(|v| v.as_str()).unwrap_or("");
        if ok && (stage == "published" || stage == "no_change") {
            es.clear_publish_approval();
            es.reset_publish_retries();
            es.save(thread_store, thread_id).await.map_err(|e| {
                format!("failed to persist publish confirmed-success state: {e}")
            })?;
            commit_phase_decision(
                thread_store,
                thread_id,
                Some(control_flow::Phase::Publish),
                PhaseDecision::forward(
                    control_flow::Phase::PostPublishReview,
                    Some(PhaseReasonCode::PublishConfirmedSuccess),
                    Some(crate::data_engineer::phase_reason_detail::publish_observation(
                        obs.clone(),
                    )),
                ),
            )
            .await?;
            return Ok(PhaseExecutorOutcome::Continue);
        }
        if ok && stage == "await_approval" {
            let retry_limit = crate::data_engineer::controller_kernel::publish_retry_limit();
            let retry_count = es.bump_publish_retry(
                crate::data_engineer::progress_controller::PublishRetryKind::AwaitApprovalLoop,
                retry_limit,
            );
            es.clear_publish_approval();
            es.save(thread_store, thread_id).await.map_err(|e| {
                format!("failed to persist publish approval fallback state: {e}")
            })?;
            if retry_count > retry_limit {
                return Err(format!(
                    "publish_await_approval_not_converged_after_retries: retries={retry_count}"
                ));
            }
            commit_phase_decision(
                thread_store,
                thread_id,
                Some(control_flow::Phase::Publish),
                PhaseDecision::loopback(
                    control_flow::Phase::PublishAwaitApproval,
                    Some(PhaseReasonCode::PublishFail),
                    Some(crate::data_engineer::phase_reason_detail::publish_failure(
                        obs,
                        retry_count,
                    )),
                ),
            )
            .await?;
            return Ok(PhaseExecutorOutcome::Continue);
        }
        let retry_limit = crate::data_engineer::controller_kernel::publish_retry_limit();
        let retry_count = es.bump_publish_retry(
            crate::data_engineer::progress_controller::PublishRetryKind::PublishFailureLoop,
            retry_limit,
        );
        es.clear_publish_approval();
        es.save(thread_store, thread_id).await.map_err(|e| {
            format!("failed to persist publish confirmed-failure state: {e}")
        })?;
        if retry_count > retry_limit {
            return Err(format!(
                "publish_confirmed_failure_not_converged_after_retries: retries={retry_count}"
            ));
        }
        commit_phase_decision(
            thread_store,
            thread_id,
            Some(control_flow::Phase::Publish),
            PhaseDecision::loopback(
                control_flow::Phase::ModelAuthor,
                Some(PhaseReasonCode::PublishConfirmedFail),
                Some(crate::data_engineer::phase_reason_detail::publish_failure(
                    obs,
                    retry_count,
                )),
            ),
        )
        .await?;
        Ok(PhaseExecutorOutcome::Continue)
    }

    pub(super) fn execute_done_phase(
        out_frames: &mut Vec<FlowFrame>,
    ) -> Result<PhaseExecutorOutcome, String> {
        let mut answer = "Agent flow completed (deterministic phases): cleanse → validate → review → model → validate → review → publish → review.\n".to_string();
        if let Some(last) = out_frames.iter().rev().find_map(|f| match f {
            FlowFrame::Review { text, .. } => Some(text.clone()),
            _ => None,
        }) {
            answer.push_str("\nLatest review summary:\n");
            answer.push_str(&last);
        }
        out_frames.push(FlowFrame::Final {
            kind: "generic".to_string(),
            payload: serde_json::json!({ "text": answer.clone() }),
            display: Some(answer),
        });
        Ok(PhaseExecutorOutcome::Return(out_frames.clone()))
    }
}

