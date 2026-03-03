use react_core::agent::AgentCtx;
use serde_json::Value;

use crate::data_engineer::controller_kernel;
use crate::data_engineer::plan::{self, ModelPlan};
use crate::data_engineer::tools::batch_sql_runner;

pub(crate) fn resolve_checklist_item_id(ctx: &AgentCtx) -> String {
    ctx.exec_ctx
        .as_ref()
        .and_then(|c| c.checklist_item_id.as_ref())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| plan::CHECKLIST_SCHEMA_CONTRACT.to_string())
}

pub(crate) async fn fail_model_schema_batch(
    ctx: &AgentCtx,
    plan: &mut ModelPlan,
    attempted_names: &[String],
    checklist_item_id: &str,
    error_message: String,
) -> Result<Value, String> {
    for name in attempted_names.iter() {
        plan::model_schema_contract_mark_needs_update(plan, name);
    }
    let budget = controller_kernel::note_batch_result_with_failure_kind(
        &mut plan.progress,
        false,
        Some(batch_sql_runner::classify_schema_batch_failure_kind(
            &error_message,
        )),
    );
    plan::save_model_plan(ctx, plan).await.map_err(|save_err| {
        format!(
            "failed to persist model schema batch failure state: {save_err}"
        )
    })?;
    if budget.exhausted() {
        return crate::data_engineer::tools::batch_contracts::to_json_value(
            crate::data_engineer::tools::batch_contracts::ModelSchemaBatchContract {
                ok: false,
                kind: Some("batch_locked".to_string()),
                reason_code: Some(serde_json::to_value(
                    controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted,
                )
                .map_err(|e| format!("failed to encode batch lock reason: {e}"))?),
                message: Some(controller_kernel::batch_lock_error_message(
                    controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted,
                )
                .to_string()),
                checklist_item_id: checklist_item_id.to_string(),
                attempted_item_names: attempted_names.to_vec(),
                succeeded_item_names: Vec::new(),
                failed_item_names: attempted_names.to_vec(),
                errors: vec![controller_kernel::batch_lock_error_message(
                    controller_kernel::BatchLockReason::ConsecutiveFailureBudgetExhausted,
                )
                .to_string()],
                progress_made: None,
                warnings: Vec::new(),
            },
        );
    }
    crate::data_engineer::tools::batch_contracts::to_json_value(
        crate::data_engineer::tools::batch_contracts::ModelSchemaBatchContract {
            ok: false,
            kind: None,
            reason_code: None,
            message: None,
            checklist_item_id: checklist_item_id.to_string(),
            attempted_item_names: attempted_names.to_vec(),
            succeeded_item_names: Vec::new(),
            failed_item_names: attempted_names.to_vec(),
            errors: vec![error_message],
            progress_made: None,
            warnings: Vec::new(),
        },
    )
}
