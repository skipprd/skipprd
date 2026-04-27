use super::*;
pub(super) fn register_batch_tool_for_plan_state(
    reg: &mut ToolRegistry,
    phase: control_flow::Phase,
    plan_state: &PlanState,
    datasets: &Option<std::sync::Arc<dyn crate::providers::DatasetCatalogProvider>>,
) -> Option<&'static str> {
    match (phase, plan_state) {
        (control_flow::Phase::CleanseAuthor, PlanState::CleanseSqlDatasetIds(_)) => {
            reg.register(tools::apply_next_batch::ApplyNextCleanseBatchTool {
                datasets: datasets.clone(),
            });
            Some("apply_next_cleanse_batch")
        }
        (control_flow::Phase::CleanseAuthor, PlanState::CleanseSchemaDatasetIds(_)) => {
            reg.register(
                tools::apply_next_schema_batch::ApplyNextCleanseSchemaBatchTool {
                    datasets: datasets.clone(),
                },
            );
            Some("apply_next_cleanse_schema_batch")
        }
        (control_flow::Phase::ModelAuthor, PlanState::ModelSqlItemNames(_)) => {
            reg.register(tools::apply_next_batch::ApplyNextModelBatchTool);
            Some("apply_next_model_batch")
        }
        (_, PlanState::Unconstrained | PlanState::Repair | PlanState::ReadOnly) => None,
        (_, PlanState::ModelSqlItemNames(_)) => None,
        (_, PlanState::CleanseSqlDatasetIds(_) | PlanState::CleanseSchemaDatasetIds(_)) => None,
    }
}

pub(super) enum FileAccessPolicy {
    ReadOnly { error_message: &'static str },
}

pub(super) struct PolicyFilesTool {
    pub(super) inner: tools::files_tool::FilesTool,
    pub(super) policy: FileAccessPolicy,
}

#[async_trait::async_trait]
impl react_core::tools::Tool for PolicyFilesTool {
    fn name(&self) -> &'static str {
        "file"
    }

    async fn call(
        &self,
        args: serde_json::Value,
        ctx: &react_core::agent::AgentCtx,
    ) -> Result<serde_json::Value, String> {
        match self.policy {
            FileAccessPolicy::ReadOnly { error_message } => {
                if !crate::tool_ops::is_file_read_op(&args) {
                    return Err(error_message.to_string());
                }
            }
        }
        self.inner.call(args, ctx).await
    }
}

/// Thread-derived guard: blocks repeated dbt_validate after failure until a mutation occurs,
/// and enforces a data probe after runtime (build/run) failures.
pub(super) struct ThreadDerivedDbtValidateTool {
    pub(super) inner: tools::dbt_validate::DbtValidateTool,
}
#[async_trait::async_trait]
impl react_core::tools::Tool for ThreadDerivedDbtValidateTool {
    fn name(&self) -> &'static str {
        "dbt_validate"
    }
    async fn call(
        &self,
        args: serde_json::Value,
        ctx: &react_core::agent::AgentCtx,
    ) -> Result<serde_json::Value, String> {
        let build = args.get("build").and_then(|v| v.as_bool()).unwrap_or(false);
        let run = args.get("run").and_then(|v| v.as_bool()).unwrap_or(false);
        let runtime_validate = build || run;
        if let (Some(store), Some(tid)) = (ctx.thread_store().as_ref(), ctx.thread_id().as_deref())
        {
            let es = crate::progress_controller::ExecutionState::load(&store.control_store(), tid)
                .await
                .map_err(|e| {
                    format!("failed to load execution state for tool policy guard: {e}")
                })?;
            if let Some(ref st) = es {
                let last_validate_failed = st.last_validate_failed();
                let mutated_since_fail = st.repair.mutated_since_fail;
                if last_validate_failed && !mutated_since_fail {
                    return Err(crate::controller_kernel::guard_block_error(
                        crate::controller_kernel::GuardReason::MutationRequiredAfterValidateFailure,
                    ));
                }
                let probe_status = st.probe_requirement_status();
                let (probe_required, probe_satisfied) = match probe_status {
                    crate::progress_controller::ProbeRequirementStatus::NotRequired => (false, true),
                    crate::progress_controller::ProbeRequirementStatus::Required => (true, false),
                    crate::progress_controller::ProbeRequirementStatus::Allowed => (true, true),
                    crate::progress_controller::ProbeRequirementStatus::ExhaustedRequireMutation => (false, true),
                };
                if runtime_validate && probe_required && !probe_satisfied {
                    return Err(crate::controller_kernel::guard_block_error(
                        crate::controller_kernel::GuardReason::ProbeRequiredAfterRuntimeFailure,
                    ));
                }
            }
        }
        self.inner.call(args, ctx).await
    }
}
