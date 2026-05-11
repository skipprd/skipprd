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

/// Tool-time access policy applied on top of the base [`tools::files_tool::FilesTool`].
///
/// The `SystemOwnedDenyList` variant consults [`crate::file_ownership::is_writable_by_llm`] for
/// every mutating op (`patch`/`write`/`rm`/`mv`) and rejects writes against system-owned and
/// build-artifact paths. Error messages include the relocation hint from
/// [`crate::file_ownership::deny_list_hint`] so a single failed write already teaches the LLM
/// where to retry.
pub(super) enum FileAccessPolicy {
    ReadOnly { error_message: &'static str },
    SystemOwnedDenyList,
}

pub(super) struct PolicyFilesTool {
    pub(super) inner: tools::files_tool::FilesTool,
    pub(super) policy: FileAccessPolicy,
}

/// Inspect `file` tool args and return all paths that would be MUTATED by this call. Get/list
/// ops return an empty Vec. `mv` returns both the source and destination.
fn mutating_target_paths(args: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let op = crate::tool_ops::classify_file_op(args);
    match op {
        crate::tool_ops::FileOpKind::Patch
        | crate::tool_ops::FileOpKind::Write
        | crate::tool_ops::FileOpKind::Rm => {
            if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
                out.push(p.to_string());
            }
        }
        crate::tool_ops::FileOpKind::Mv => {
            if let Some(p) = args.get("from").and_then(|v| v.as_str()) {
                out.push(p.to_string());
            }
            if let Some(p) = args.get("to").and_then(|v| v.as_str()) {
                out.push(p.to_string());
            }
        }
        crate::tool_ops::FileOpKind::Get
        | crate::tool_ops::FileOpKind::List
        | crate::tool_ops::FileOpKind::Other => {}
    }
    out
}

/// Format the deny-list error shown to the LLM, embedding the relocation hint when one exists.
fn format_deny_list_error(op: crate::tool_ops::FileOpKind, rel: &str) -> String {
    let hint = crate::file_ownership::deny_list_hint(rel).unwrap_or(
        "This path is system-managed and cannot be modified by the agent.",
    );
    format!(
        "file op={} denied for '{}': {}",
        op.as_str(),
        rel.trim_start_matches('/'),
        hint
    )
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
            FileAccessPolicy::SystemOwnedDenyList => {
                let op = crate::tool_ops::classify_file_op(&args);
                for rel in mutating_target_paths(&args) {
                    if !crate::file_ownership::is_writable_by_llm(&rel) {
                        return Err(format_deny_list_error(op, &rel));
                    }
                }
            }
        }
        self.inner.call(args, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_ops::FileOpKind;
    use serde_json::json;

    #[test]
    fn mutating_target_paths_returns_path_for_patch_write_rm() {
        for op in ["patch", "write", "rm"] {
            let args = json!({"op": op, "path": "profiles.yml"});
            let paths = mutating_target_paths(&args);
            assert_eq!(paths, vec!["profiles.yml".to_string()], "op {op}");
        }
    }

    #[test]
    fn mutating_target_paths_returns_from_and_to_for_mv() {
        let args = json!({"op": "mv", "from": "a.sql", "to": "b.sql"});
        let paths = mutating_target_paths(&args);
        assert_eq!(paths, vec!["a.sql".to_string(), "b.sql".to_string()]);
    }

    #[test]
    fn mutating_target_paths_empty_for_read_ops() {
        for op in ["get", "list"] {
            let args = json!({"op": op, "path": "profiles.yml"});
            assert!(mutating_target_paths(&args).is_empty(), "op {op}");
        }
    }

    #[test]
    fn format_deny_list_error_embeds_hint_for_profiles_yml() {
        let msg = format_deny_list_error(FileOpKind::Patch, "profiles.yml");
        assert!(msg.contains("profiles.yml"));
        assert!(msg.contains("system-scoped"));
        assert!(msg.contains("op=patch"));
    }

    #[test]
    fn format_deny_list_error_uses_generic_fallback_for_unknown_paths() {
        // A writable path should not be passed in practice, but the function shouldn't panic.
        let msg = format_deny_list_error(FileOpKind::Write, "models/x.sql");
        assert!(msg.contains("models/x.sql"));
    }

    #[test]
    fn format_deny_list_error_embeds_hint_for_packages_yml() {
        let msg = format_deny_list_error(FileOpKind::Write, "packages.yml");
        assert!(msg.contains("packages.yml"));
        assert!(msg.contains("governance-controlled"));
        assert!(msg.contains("op=write"));
    }

    #[test]
    fn format_deny_list_error_embeds_hint_for_target_build_artifact() {
        let msg = format_deny_list_error(FileOpKind::Rm, "target/manifest.json");
        assert!(msg.contains("target/manifest.json"));
        assert!(msg.contains("Build artifact"));
        assert!(msg.contains("op=rm"));
    }

    // Mutating ops against system-owned paths must short-circuit before reaching the inner
    // FilesTool. We assert by checking that the LLM-visible error contains the expected hint
    // text — the inner tool is never invoked because policy returns Err first.
    #[tokio::test]
    async fn policy_file_tool_denies_write_to_profiles_yml_with_hint() {
        use react_core::agent::{AgentCtxBuilder, DefaultPolicy};
        use react_core::keyspace::{DefaultKeyspace, Keyspace};
        use react_core::llm::{LargeLanguageModel, NullModel};
        use react_core::scope::RequestScope;
        use react_core::storage::StorageAdapter;
        use react_core::tools::Tool;
        use react_module_storage_memory::InMemoryStorageAdapter;
        use std::sync::Arc;

        let policy_tool = PolicyFilesTool {
            inner: tools::files_tool::FilesTool { datasets: None },
            policy: FileAccessPolicy::SystemOwnedDenyList,
        };
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let llm: Arc<dyn LargeLanguageModel> = Arc::new(NullModel::new());
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        let ctx = AgentCtxBuilder::new(
            llm,
            storage,
            RequestScope::parse("t", "w", "p").expect("scope"),
            keyspace,
            Arc::new(DefaultPolicy),
        )
        .top_k(1)
        .per_step_timeout_secs(1)
        .max_steps(1)
        .thread_id("test".to_string())
        .agent_name("test".to_string())
        .build();

        let err = policy_tool
            .call(json!({"op": "patch", "path": "profiles.yml", "patch_text": "@@ \n"}), &ctx)
            .await
            .expect_err("write to profiles.yml must be denied");
        assert!(err.contains("profiles.yml"), "error: {err}");
        assert!(err.contains("system-scoped"), "error: {err}");
        assert!(err.contains("op=patch"), "error: {err}");
    }

    #[tokio::test]
    async fn policy_file_tool_allows_write_to_dbt_project_yml_through_to_inner() {
        // dbt_project.yml is classified `Shared`: writes pass through the policy and the sanitizer
        // strips skippr-owned top-level keys on save. Verify the policy does NOT short-circuit.
        use react_core::agent::{AgentCtxBuilder, DefaultPolicy};
        use react_core::keyspace::{DefaultKeyspace, Keyspace};
        use react_core::llm::{LargeLanguageModel, NullModel};
        use react_core::scope::RequestScope;
        use react_core::storage::StorageAdapter;
        use react_core::tools::Tool;
        use react_module_storage_memory::InMemoryStorageAdapter;
        use std::sync::Arc;

        let policy_tool = PolicyFilesTool {
            inner: tools::files_tool::FilesTool { datasets: None },
            policy: FileAccessPolicy::SystemOwnedDenyList,
        };
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let llm: Arc<dyn LargeLanguageModel> = Arc::new(NullModel::new());
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        let ctx = AgentCtxBuilder::new(
            llm,
            storage,
            RequestScope::parse("t", "w", "p").expect("scope"),
            keyspace,
            Arc::new(DefaultPolicy),
        )
        .top_k(1)
        .per_step_timeout_secs(1)
        .max_steps(1)
        .thread_id("test".to_string())
        .agent_name("test".to_string())
        .build();

        // Any error returned MUST originate from the inner tool, not from the deny-list. The
        // unmistakable deny-list signature `op=patch denied for '...'` must NOT be present.
        let result = policy_tool
            .call(
                json!({"op": "patch", "path": "dbt_project.yml", "patch_text": "@@ \n"}),
                &ctx,
            )
            .await;
        if let Err(err) = &result {
            assert!(
                !err.contains("denied for 'dbt_project.yml'"),
                "policy must not deny dbt_project.yml; got: {err}"
            );
        }
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
