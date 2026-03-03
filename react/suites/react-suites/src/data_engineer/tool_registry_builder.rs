use super::*;

impl DataEngineerSuite {
    pub(super) fn build_tools(agent_mode: AgentMode, sctx: &SuiteCtx) -> Result<ToolRegistry, String> {
        use crate::data_engineer::tools::{
            artifacts::ArtifactsTool, files_tool::FilesTool, sql_run::SqlRunTool,
            sql_sample::SqlSampleTool, sql_schema::SqlSchemaTool, sql_stats::SqlStatsTool,
            vect_query::VectQueryTool,
        };

        /// Thread-derived guard: blocks repeated dbt_validate after failure until a mutation occurs,
        /// and enforces a data probe after runtime (build/run) failures.
        struct ThreadDerivedDbtValidateTool {
            inner: tools::dbt_validate::DbtValidateTool,
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
                if let (Some(store), Some(tid)) =
                    (ctx.thread_store.as_ref(), ctx.thread_id.as_deref())
                {
                    let guard = crate::data_engineer::progress_controller::ExecutionState::load(
                        store, tid,
                    )
                    .await
                    .map(|st| {
                        crate::data_engineer::control_flow::derive_guard_state_from_execution_state(
                            &st,
                        )
                    })
                    .unwrap_or_default();
                    if guard.last_validate_failed && !guard.mutated_since_fail {
                        return Err(crate::data_engineer::controller_kernel::guard_block_error(
                            crate::data_engineer::controller_kernel::GuardReason::MutationRequiredAfterValidateFailure,
                        ));
                    }
                    if runtime_validate && guard.probe_required && !guard.probe_satisfied {
                        return Err(crate::data_engineer::controller_kernel::guard_block_error(
                            crate::data_engineer::controller_kernel::GuardReason::ProbeRequiredAfterRuntimeFailure,
                        ));
                    }
                }
                self.inner.call(args, ctx).await
            }
        }

        let mut registry = ToolRegistry::new();

        let query = sctx
            .query
            .as_ref()
            .ok_or_else(|| "query provider missing".to_string())?
            .clone();

        // Shared analytics tools (note: run_sql is registered per-agent so authoring agents can be guarded)
        registry.register(SqlSchemaTool {
            query: query.clone(),
            datasets: sctx.datasets.clone(),
            catalog: sctx.catalog.clone(),
        });
        registry.register(SqlStatsTool {
            catalog: sctx.catalog.clone(),
            datasets: sctx.datasets.clone(),
        });
        registry.register(SqlSampleTool {
            query: query.clone(),
        });
        registry.register(VectQueryTool);

        let allow_user_interrupt_tools = !Self::headless_mode_enabled();
        let caps = Self::agent_capability_profile(agent_mode, allow_user_interrupt_tools);

        // Allow review to read the current dbt project state (manifest/schema/models)
        // without permitting writes.
        struct ReadOnlyFilesTool {
            inner: FilesTool,
        }
        #[async_trait::async_trait]
        impl react_core::tools::Tool for ReadOnlyFilesTool {
            fn name(&self) -> &'static str {
                "file"
            }
            async fn call(
                &self,
                args: serde_json::Value,
                ctx: &react_core::agent::AgentCtx,
            ) -> Result<serde_json::Value, String> {
                if !crate::data_engineer::tool_ops::is_file_read_op(&args) {
                    return Err(
                        "file is read-only for review; use op='get' or op='list' (mutating ops are disabled: patch/rm/mv)".to_string(),
                    );
                }
                self.inner.call(args, ctx).await
            }
        }

        if caps.contains(&AgentToolCapability::ReadOnlyFile) {
            registry.register(ReadOnlyFilesTool {
                inner: FilesTool {
                    datasets: sctx.datasets.clone(),
                },
            });
        } else if caps.contains(&AgentToolCapability::MutableFile) {
            registry.register(FilesTool {
                datasets: sctx.datasets.clone(),
            });
        }
        if caps.contains(&AgentToolCapability::RunSql) {
            registry.register(SqlRunTool {
                query: query.clone(),
            });
        }
        if caps.contains(&AgentToolCapability::AskUser) {
            registry.register(tools::ask_user::AskUserTool);
        }
        if caps.contains(&AgentToolCapability::AskApproval) {
            registry.register(tools::ask_approval::AskApprovalTool);
        }
        if caps.contains(&AgentToolCapability::SearchDbtExamples) {
            registry.register(tools::dbt_examples::SearchDbtExamplesTool);
        }
        if caps.contains(&AgentToolCapability::StagingModel) {
            registry.register(tools::staging_model::StagingModelTool {
                datasets: sctx.datasets.clone(),
            });
        }
        if caps.contains(&AgentToolCapability::GoldModel) {
            registry.register(tools::gold_model::GoldModelTool);
        }
        if caps.contains(&AgentToolCapability::DbtValidate) {
            registry.register(ThreadDerivedDbtValidateTool {
                inner: tools::dbt_validate::DbtValidateTool {
                    datasets: sctx.datasets.clone(),
                    catalog: sctx.catalog.clone(),
                },
            });
        }
        if caps.contains(&AgentToolCapability::PublishDbt) {
            registry.register(tools::publish_dbt_to_provider::PublishDbtToProviderTool {
                datasets: sctx.datasets.clone(),
                catalog: sctx.catalog.clone(),
            });
        }
        if caps.contains(&AgentToolCapability::SqlRegister) {
            registry.register(tools::sql_register::SqlRegisterTool);
        }
        if caps.contains(&AgentToolCapability::CatalogNote) {
            registry.register(tools::catalog_note::CatalogNoteTool);
        }
        if caps.contains(&AgentToolCapability::Artifacts) {
            registry.register(ArtifactsTool);
        }

        Ok(registry)
    }

    pub(super) fn build_tools_card_for_agent_type(agent_mode: AgentMode) -> String {
        let allow_user_interrupt_tools = !Self::headless_mode_enabled();
        let caps = Self::agent_capability_profile(agent_mode, allow_user_interrupt_tools);
        match agent_mode {
            AgentMode::Review => Self::build_tools_card(
                "Allowed tools (review mode, read-only):",
                vec![
                    "- file(args:{op:\"list\"|\"get\", prefix?:string, path?:string, limit?:int, max_chars?:int})".to_string(),
                    "- sql_schema / sql_stats / sql_sample / vect_query (read-only context)".to_string(),
                    "- artifacts".to_string(),
                ],
                Vec::new(),
                Some(
                    "Not available: run_sql, staging_model, gold_model, file patch/rm/mv, dbt_validate, publish_dbt_to_provider."
                        .to_string(),
                ),
            ),
            AgentMode::Ask => {
                let mut lines = vec!["- file(args:{op:\"list\"|\"get\"|\"patch\"|\"rm\"|\"mv\", ...})".to_string(),
                    "- sql_schema / sql_stats / sql_sample / vect_query (discovery context)".to_string()];
                if caps.contains(&AgentToolCapability::RunSql) {
                    lines.push("- run_sql(args:{sql:string})".to_string());
                }
                if caps.contains(&AgentToolCapability::AskApproval) {
                    lines.push("- ask_approval(args:{prompt:string})".to_string());
                }
                if caps.contains(&AgentToolCapability::Artifacts) {
                    lines.push("- artifacts".to_string());
                }
                if caps.contains(&AgentToolCapability::AskUser) {
                    lines.push("- ask_user(args:{prompt:string})".to_string());
                }
                Self::build_tools_card("Allowed tools (ask mode):", lines, Vec::new(), None)
            }
            AgentMode::Cleanse => {
                let mut lines = vec!["- file(args:{op:\"list\"|\"get\"|\"patch\"|\"rm\"|\"mv\", ...})".to_string(),
                    "- sql_schema / sql_stats / sql_sample / vect_query (discovery context)".to_string()];
                if caps.contains(&AgentToolCapability::RunSql) {
                    lines.push("- run_sql(args:{sql:string})".to_string());
                }
                if caps.contains(&AgentToolCapability::StagingModel) {
                    lines.push("- staging_model(args:{dataset_ids:[string], instructions?:string, sql?:string|staging_model?:string|expression?:string})".to_string());
                }
                if caps.contains(&AgentToolCapability::DbtValidate)
                    || caps.contains(&AgentToolCapability::PublishDbt)
                {
                    lines.push("- dbt_validate / publish_dbt_to_provider".to_string());
                }
                if caps.contains(&AgentToolCapability::AskApproval) {
                    lines.push("- ask_approval(args:{prompt:string})".to_string());
                }
                if caps.contains(&AgentToolCapability::Artifacts) {
                    lines.push("- artifacts".to_string());
                }
                if caps.contains(&AgentToolCapability::AskUser) {
                    lines.push("- ask_user(args:{prompt:string})".to_string());
                }
                Self::build_tools_card("Allowed tools (cleanse mode):", lines, Vec::new(), None)
            }
            AgentMode::Model | AgentMode::Agent => {
                let mut lines = vec!["- file(args:{op:\"list\"|\"get\"|\"patch\"|\"rm\"|\"mv\", ...})".to_string(),
                    "- sql_schema / sql_stats / sql_sample / vect_query (discovery context)".to_string()];
                if caps.contains(&AgentToolCapability::RunSql) {
                    lines.push("- run_sql(args:{sql:string})".to_string());
                }
                if caps.contains(&AgentToolCapability::StagingModel)
                    || caps.contains(&AgentToolCapability::GoldModel)
                {
                    lines.push("- staging_model / gold_model".to_string());
                }
                if caps.contains(&AgentToolCapability::DbtValidate)
                    || caps.contains(&AgentToolCapability::PublishDbt)
                {
                    lines.push("- dbt_validate / publish_dbt_to_provider".to_string());
                }
                if caps.contains(&AgentToolCapability::AskApproval) {
                    lines.push("- ask_approval(args:{prompt:string})".to_string());
                }
                if caps.contains(&AgentToolCapability::Artifacts) {
                    lines.push("- artifacts".to_string());
                }
                if caps.contains(&AgentToolCapability::AskUser) {
                    lines.push("- ask_user(args:{prompt:string})".to_string());
                }
                Self::build_tools_card("Allowed tools (model mode):", lines, Vec::new(), None)
            }
        }
    }

    pub(super) fn build_tools_for_phase(
        phase: control_flow::Phase,
        guard: &control_flow::DerivedGuardState,
        _allow_ask_approval: bool,
        sctx: &SuiteCtx,
        allowed_batch: Option<AllowedBatch>,
        single_target_repair_path: Option<String>,
        suppress_manifest_json_in_plan: bool,
    ) -> Result<(ToolRegistry, String), String> {
        use crate::data_engineer::tools::{
            artifacts::ArtifactsTool, files_tool::FilesTool, json_file::JsonFileTool,
            sql_run::SqlRunTool, sql_sample::SqlSampleTool, sql_schema::SqlSchemaTool,
            sql_stats::SqlStatsTool, vect_query::VectQueryTool,
        };

        let query = sctx
            .query
            .as_ref()
            .ok_or_else(|| "query provider missing".to_string())?
            .clone();

        let mut reg = ToolRegistry::new();

        // Common read tools (safe in most phases)
        reg.register(SqlSchemaTool {
            query: query.clone(),
            datasets: sctx.datasets.clone(),
            catalog: sctx.catalog.clone(),
        });
        reg.register(SqlStatsTool {
            catalog: sctx.catalog.clone(),
            datasets: sctx.datasets.clone(),
        });
        reg.register(SqlSampleTool {
            query: query.clone(),
        });
        reg.register(VectQueryTool);
        reg.register(ArtifactsTool);

        let tools_card: String;

        match phase {
            control_flow::Phase::CleansePlan | control_flow::Phase::ModelPlan => {
                // Plan phases: read-only discovery + (optional) probes. No dbt file mutations.
                let suppress_manifest_json =
                    suppress_manifest_json_in_plan && phase == control_flow::Phase::ModelPlan;
                reg.register(SqlRunTool {
                    query: query.clone(),
                });
                reg.register(tools::dbt_examples::SearchDbtExamplesTool);

                // Read-only file tool (no patch).
                struct ReadOnlyFilesTool {
                    inner: FilesTool,
                }
                #[async_trait::async_trait]
                impl react_core::tools::Tool for ReadOnlyFilesTool {
                    fn name(&self) -> &'static str {
                        "file"
                    }
                    async fn call(
                        &self,
                        args: serde_json::Value,
                        ctx: &react_core::agent::AgentCtx,
                    ) -> Result<serde_json::Value, String> {
                        if !crate::data_engineer::tool_ops::is_file_read_op(&args) {
                            return Err(
                                "file is read-only in plan phases; use op='get' or op='list' (mutating ops are disabled: patch/rm/mv)".to_string(),
                            );
                        }
                        self.inner.call(args, ctx).await
                    }
                }
                reg.register(ReadOnlyFilesTool {
                    inner: FilesTool {
                        datasets: sctx.datasets.clone(),
                    },
                });
                if !suppress_manifest_json {
                    reg.register(JsonFileTool);
                }

                let mut tool_lines: Vec<String> = vec![
                    "- file(args:{op:\"list\", prefix?:string, limit?:int} | {op:\"get\", path:string, max_chars?:int})".to_string(),
                    "- sql_schema / sql_stats / sql_sample / vect_query (discovery context)".to_string(),
                    "- run_sql (targeted probes)".to_string(),
                    "- artifacts".to_string(),
                ];
                if suppress_manifest_json {
                    tool_lines.push("- json_file is temporarily disabled for this model_plan retry due to repeated manifest lookup failures; use deterministic fallback evidence (file + sql_schema + sql_stats/sql_sample).".to_string());
                } else {
                    tool_lines.push("- json_file(args:{op:\"get_item\", path:string, pointer?:string} | {op:\"query\", path:string, pointer?:string, unique_id?:string, name?:string, resource_type?:string, limit?:int})".to_string());
                    tool_lines.push("  - IMPORTANT: use args.op (NOT args.type). For list use args.prefix (NOT path:\".\").".to_string());
                    tool_lines.push("  - For manifest queries use canonical path: target/manifest.json (NOT manifest.json).".to_string());
                }
                tools_card = Self::build_tools_card(
                    "Allowed tools (plan phase, read-only):",
                    tool_lines,
                    Vec::new(),
                    Some(
                        "Not available: staging_model, gold_model, file patch/rm/mv, dbt_validate, publish_dbt_to_provider.".to_string(),
                    ),
                );
            }
            control_flow::Phase::CleanseAuthor | control_flow::Phase::ModelAuthor => {
                // Authoring phases: allow investigation + mutations; validation/publish are suite-driven.
                //
                // Deterministic repair hard-cutover:
                // - classic gate: after validate fail with no mutation yet, require mutation next.
                // - single-target repair mode: ALWAYS require mutation next, even if a prior mutation
                //   already happened in this repair cycle (prevents read/get loops against removed targets).
                let hard_mutation_only = (guard.last_validate_failed && !guard.mutated_since_fail)
                    || single_target_repair_path.is_some();
                let allow_probe_sql = guard.probe_required && !guard.probe_satisfied;
                let plan_batched_cleanse_sql = phase == control_flow::Phase::CleanseAuthor
                    && matches!(allowed_batch, Some(AllowedBatch::CleanseSqlDatasetIds(_)));
                let plan_batched_cleanse_schema = phase == control_flow::Phase::CleanseAuthor
                    && matches!(allowed_batch, Some(AllowedBatch::CleanseSchemaDatasetIds(_)));
                let plan_batched_model_sql = phase == control_flow::Phase::ModelAuthor
                    && matches!(allowed_batch, Some(AllowedBatch::ModelSqlItemNames(_)));
                let plan_batched_model_schema = phase == control_flow::Phase::ModelAuthor
                    && matches!(allowed_batch, Some(AllowedBatch::ModelSchemaItemNames(_)));
                let authoring_policy = crate::data_engineer::authoring_driver::derive_authoring_tool_policy(
                    crate::data_engineer::authoring_driver::AuthoringToolPolicyInput {
                        hard_mutation_only,
                        single_target_repair: single_target_repair_path.is_some(),
                        allow_probe_sql,
                        plan_batched_cleanse_sql,
                        plan_batched_cleanse_schema,
                        plan_batched_model_sql,
                        plan_batched_model_schema,
                    },
                );

                if hard_mutation_only {
                    // Mutation-only file tool to avoid "read-only thrash" when we require a mutation next.
                    struct PutOnlyFilesTool {
                        inner: FilesTool,
                        single_target_path: Option<String>,
                    }
                    #[async_trait::async_trait]
                    impl react_core::tools::Tool for PutOnlyFilesTool {
                        fn name(&self) -> &'static str {
                            "file"
                        }
                        async fn call(
                            &self,
                            args: serde_json::Value,
                            ctx: &react_core::agent::AgentCtx,
                        ) -> Result<serde_json::Value, String> {
                            let op = args.get("op").and_then(|x| x.as_str()).unwrap_or("get");
                            let is_repair_mutation =
                                crate::data_engineer::tool_ops::is_file_repair_mutation_op(&args);
                            if self.single_target_path.is_some() && !is_repair_mutation {
                                return Err("file is in deterministic single-target repair mode; only op='patch'|'rm'|'mv' is allowed.".to_string());
                            }
                            if self.single_target_path.is_none()
                                && !is_repair_mutation
                            {
                                return Err("file is mutation-only right now (a mutating fix is required before any further validation). Allowed ops: patch/rm/mv.".to_string());
                            }
                            if let Some(want) = self.single_target_path.as_ref() {
                                fn collect_paths(v: &serde_json::Value) -> Vec<String> {
                                    let mut out: Vec<String> = Vec::new();
                                    if let Some(p) = v.get("path").and_then(|x| x.as_str()) {
                                        let p = p.trim();
                                        if !p.is_empty() {
                                            out.push(p.to_string());
                                        }
                                    }
                                    if let Some(p) = v.get("from").and_then(|x| x.as_str()) {
                                        let p = p.trim();
                                        if !p.is_empty() {
                                            out.push(p.to_string());
                                        }
                                    }
                                    out
                                }
                                let mut paths = collect_paths(&args);
                                paths.sort();
                                paths.dedup();
                                if paths.is_empty() {
                                    return Err(format!(
                                        "file deterministic repair mode requires explicit path/from='{}'.",
                                        want
                                    ));
                                }
                                if paths.len() != 1 || paths[0] != *want {
                                    return Err(format!(
                                        "file deterministic single-target repair mode violation: only '{}' may be mutated right now (got: {}).",
                                        want,
                                        paths.join(", ")
                                    ));
                                }
                            }
                            // Deterministic repair ladder enforcement (hard cutover).
                            if let (Some(store), Some(thread_id), Some(want)) = (
                                ctx.thread_store.as_ref(),
                                ctx.thread_id.as_deref(),
                                self.single_target_path.as_ref(),
                            ) {
                                if is_repair_mutation {
                                    let es = crate::data_engineer::progress_controller::ExecutionState::load(
                                        store, thread_id,
                                    )
                                    .await
                                    .unwrap_or_else(
                                        crate::data_engineer::progress_controller::ExecutionState::new,
                                    );

                                    match es.ladder_step {
                                        crate::data_engineer::progress_controller::RepairLadderStep::Stop => {
                                            return Err(format!(
                                                "deterministic repair ladder stop: '{}' did not converge after prior repair attempts. Stop and apply a manual fix for '{}' before re-running.",
                                                want, want
                                            ));
                                        }
                                        crate::data_engineer::progress_controller::RepairLadderStep::ReplaceFile => {
                                            if op == "patch" {
                                                // Hard cutover: Cursor/Aider hunks-only patches only.
                                                let patch_text = args
                                                    .get("patch_text")
                                                    .and_then(|v| v.as_str())
                                                    .unwrap_or("");
                                                let has_patch_text = !patch_text.trim().is_empty()
                                                    && patch_text.trim_start().starts_with("@@");
                                                let guard_path_ok = args
                                                    .get("path")
                                                    .and_then(|v| v.as_str())
                                                    .map(|p| p.trim() == want)
                                                    .unwrap_or(false);
                                                if patch_text.contains("@@ ... @@") {
                                                    return Err(format!(
                                                        "deterministic repair ladder step for '{}': placeholder hunk header '@@ ... @@' is not allowed. Use real hunks with exact context lines from the current file content.",
                                                        want
                                                    ));
                                                }
                                                if !has_patch_text || !guard_path_ok {
                                                    return Err(format!(
                                                        "deterministic repair ladder step requires a guarded single-file patch for '{}': args must include path='{}' + patch_text starting with '@@' (Cursor/Aider hunks-only; no ---/+++ headers).",
                                                        want,
                                                        want
                                                    ));
                                                }
                                            }
                                        }
                                        crate::data_engineer::progress_controller::RepairLadderStep::PatchTarget => {}
                                    }
                                }
                            }

                            let res = self.inner.call(args.clone(), ctx).await;

                            // Update repair state after the attempt (best-effort, but should fail fast if persistence breaks).
                            if let (Some(store), Some(thread_id), Some(want)) = (
                                ctx.thread_store.as_ref(),
                                ctx.thread_id.as_deref(),
                                self.single_target_path.as_ref(),
                            ) {
                                if is_repair_mutation {
                                    let mut es =
                                        crate::data_engineer::progress_controller::ExecutionState::load(
                                            store, thread_id,
                                        )
                                        .await
                                        .unwrap_or_else(
                                            crate::data_engineer::progress_controller::ExecutionState::new,
                                        );
                                    if es.target_path.as_deref().unwrap_or("").trim().is_empty() {
                                        es.target_path = Some(want.clone());
                                    }

                                    match &res {
                                        Ok(v) => {
                                            let ok = v
                                                .get("ok")
                                                .and_then(|x| x.as_bool())
                                                .unwrap_or(false);
                                            let mutated = v
                                                .get("mutated")
                                                .and_then(|x| x.as_bool())
                                                .unwrap_or(false);
                                            es.note_patch_attempt(ok, mutated);
                                        }
                                        Err(_) => {
                                            es.note_patch_attempt(false, false);
                                        }
                                    }
                                    // Persist state; hard fail if we cannot persist during repair mode.
                                    es.save(store, thread_id).await?;
                                }
                            }

                            res
                        }
                    }
                    reg.register(PutOnlyFilesTool {
                        inner: FilesTool {
                            datasets: sctx.datasets.clone(),
                        },
                        single_target_path: single_target_repair_path.clone(),
                    });

                    // Keep targeted probes available: probe requirements can be asserted after runtime failures,
                    // and those probes must be satisfiable even when the next step must be a mutation.
                    struct ProbeAwareRunSqlTool {
                        inner: SqlRunTool,
                    }
                    #[async_trait::async_trait]
                    impl react_core::tools::Tool for ProbeAwareRunSqlTool {
                        fn name(&self) -> &'static str {
                            "run_sql"
                        }
                        async fn call(
                            &self,
                            args: serde_json::Value,
                            ctx: &react_core::agent::AgentCtx,
                        ) -> Result<serde_json::Value, String> {
                            let sql = args
                                .get("sql")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            if let (Some(store), Some(thread_id)) =
                                (ctx.thread_store.as_ref(), ctx.thread_id.as_deref())
                            {
                                let mut es = crate::data_engineer::progress_controller::ExecutionState::load(
                                    store, thread_id,
                                )
                                .await
                                .unwrap_or_else(
                                    crate::data_engineer::progress_controller::ExecutionState::new,
                                );
                                if matches!(
                                    es.probe_requirement_status(),
                                    crate::data_engineer::progress_controller::ProbeRequirementStatus::ExhaustedRequireMutation
                                ) {
                                    return Err("run_sql probe loop exhausted for this validate-failure cycle; apply a mutating file fix before probing again.".to_string());
                                }
                                let res = self.inner.call(args, ctx).await;
                                if es.last_validate_ok == Some(false) && es.hard_mutation_repair_mode {
                                    match &res {
                                        Ok(v) => {
                                            let ok =
                                                v.get("ok").and_then(|x| x.as_bool()).unwrap_or(false);
                                            let sig = crate::data_engineer::progress_controller::ProbeSignature::from_run_sql(
                                                &sql, v,
                                            );
                                            let _ = es.note_probe_attempt(&sql, ok, sig);
                                        }
                                        Err(_) => {
                                            let sig = crate::data_engineer::progress_controller::ProbeSignature::from_run_sql(
                                                &sql,
                                                &serde_json::json!({}),
                                            );
                                            let _ = es.note_probe_attempt(&sql, false, sig);
                                        }
                                    }
                                    es.save(store, thread_id).await?;
                                }
                                return res;
                            }
                            self.inner.call(args, ctx).await
                        }
                    }
                    if allow_probe_sql {
                        reg.register(ProbeAwareRunSqlTool {
                            inner: SqlRunTool {
                                query: query.clone(),
                            },
                        });
                    }

                    // In hard_mutation_only mode, only expose schema-batch tools when we are in
                    // schema repair mode (single_target_repair_path is absent). SQL-target repair
                    // mode must stay file-targeted to avoid schema-tool no-op loops.
                    let mut tool_lines: Vec<String> = Vec::new();
                    if phase == control_flow::Phase::CleanseAuthor
                        && !matches!(
                            authoring_policy,
                            crate::data_engineer::authoring_driver::AuthoringToolPolicy::HardMutationSingleTarget
                        )
                    {
                        reg.register(
                            tools::apply_next_schema_batch::ApplyNextCleanseSchemaBatchTool {
                                datasets: sctx.datasets.clone(),
                            },
                        );
                        tool_lines.push(
                            "- apply_next_cleanse_schema_batch(args:{instructions?:string})"
                                .to_string(),
                        );
                    }
                    if phase == control_flow::Phase::ModelAuthor
                        && !matches!(
                            authoring_policy,
                            crate::data_engineer::authoring_driver::AuthoringToolPolicy::HardMutationSingleTarget
                        )
                    {
                        reg.register(
                            tools::apply_next_schema_batch::ApplyNextModelSchemaBatchTool {
                                datasets: sctx.datasets.clone(),
                            },
                        );
                        tool_lines.push(
                            "- apply_next_model_schema_batch(args:{instructions?:string})"
                                .to_string(),
                        );
                    }
                    tool_lines.extend_from_slice(&[
                        "- file(args:{op:\"patch\"|\"rm\"|\"mv\", ...})".to_string(),
                        "  - op=patch args: {path:string, patch_text:string} (Cursor/Aider hunks-only; patch_text starts with '@@' and MUST NOT include ---/+++ or diff --git)".to_string(),
                        "  - op=rm args: {path:string, expected_sha256?:string}".to_string(),
                        "  - op=mv args: {from:string, to:string, expected_sha256?:string}".to_string(),
                    ]);
                    if allow_probe_sql {
                        tool_lines.push("- run_sql(args:{sql:string}) (targeted probes are currently required by probe gate)".to_string());
                    }
                    if single_target_repair_path.is_some() {
                        tool_lines.push("Deterministic single-target repair mode is active: only file op=patch/rm/mv targeting the current failing model file is allowed.".to_string());
                    }
                    tools_card = Self::build_tools_card(
                        "Allowed tools (authoring phase; HARD constraint: mutation required next):",
                        tool_lines,
                        Vec::new(),
                        Some(
                            "Not available: read/explore tools, dbt_validate, publish_dbt_to_provider."
                                .to_string(),
                        ),
                    );
                } else {
                    // Normal authoring: allow read/explore + probes.
                    if phase == control_flow::Phase::CleanseAuthor {
                        if let Some(ab) = allowed_batch.clone() {
                            match ab {
                                AllowedBatch::CleanseSqlDatasetIds(_) => {
                                    reg.register(tools::apply_next_batch::ApplyNextCleanseBatchTool {
                                        datasets: sctx.datasets.clone(),
                                    });
                                }
                                AllowedBatch::CleanseSchemaDatasetIds(_) => {
                                    reg.register(
                                        tools::apply_next_schema_batch::ApplyNextCleanseSchemaBatchTool {
                                            datasets: sctx.datasets.clone(),
                                        },
                                    );
                                }
                                _ => {}
                            }
                        } else {
                            reg.register(tools::staging_model::StagingModelTool {
                                datasets: sctx.datasets.clone(),
                            });
                            reg.register(
                                tools::apply_next_schema_batch::ApplyNextCleanseSchemaBatchTool {
                                    datasets: sctx.datasets.clone(),
                                },
                            );
                        }
                    }
                    if phase == control_flow::Phase::ModelAuthor {
                        if let Some(ab) = allowed_batch.clone() {
                            match ab {
                                AllowedBatch::ModelSqlItemNames(_) => {
                                    reg.register(tools::apply_next_batch::ApplyNextModelBatchTool);
                                }
                                AllowedBatch::ModelSchemaItemNames(_) => {
                                    reg.register(
                                        tools::apply_next_schema_batch::ApplyNextModelSchemaBatchTool {
                                            datasets: sctx.datasets.clone(),
                                        },
                                    );
                                }
                                _ => {}
                            }
                        } else {
                            reg.register(tools::gold_model::GoldModelTool);
                            reg.register(
                                tools::apply_next_schema_batch::ApplyNextModelSchemaBatchTool {
                                    datasets: sctx.datasets.clone(),
                                },
                            );
                        }
                    }
                    reg.register(SqlRunTool {
                        query: query.clone(),
                    });
                    reg.register(tools::dbt_examples::SearchDbtExamplesTool);
                    reg.register(FilesTool {
                        datasets: sctx.datasets.clone(),
                    });
                    reg.register(JsonFileTool);

                    if matches!(
                        authoring_policy,
                        crate::data_engineer::authoring_driver::AuthoringToolPolicy::BatchingCleanseSql
                    ) {
                        let lines = vec![
                            "- apply_next_cleanse_batch(args:{instructions?:string})".to_string(),
                            "- file(args:{op:\"list\"|\"get\", prefix?:string, path?:string, limit?:int, max_chars?:int} | {op:\"patch\", path:string, patch_text:string} | {op:\"rm\", path:string, expected_sha256?:string} | {op:\"mv\", from:string, to:string, expected_sha256?:string})".to_string(),
                            "- json_file(args:{op:\"get_item\", path:string, pointer?:string} | {op:\"query\", path:string, pointer?:string, unique_id?:string, name?:string, resource_type?:string, limit?:int})".to_string(),
                            "- sql_schema / sql_stats / sql_sample / vect_query (discovery context)"
                                .to_string(),
                            "- run_sql (targeted probes)".to_string(),
                            "- artifacts".to_string(),
                        ];
                        tools_card = Self::build_tools_card(
                            "Allowed tools (authoring phase; plan-batched, deterministic):",
                            lines,
                            Vec::new(),
                            Some("Not available in this phase: staging_model (batch tool calls it deterministically), dbt_validate, publish_dbt_to_provider.".to_string()),
                        );
                    } else if matches!(
                        authoring_policy,
                        crate::data_engineer::authoring_driver::AuthoringToolPolicy::BatchingCleanseSchema
                    ) {
                        let lines = vec![
                            "- apply_next_cleanse_schema_batch(args:{instructions?:string})"
                                .to_string(),
                            "- file(args:{op:\"list\"|\"get\", prefix?:string, path?:string, limit?:int, max_chars?:int} | {op:\"patch\", path:string, patch_text:string} | {op:\"rm\", path:string, expected_sha256?:string} | {op:\"mv\", from:string, to:string, expected_sha256?:string})".to_string(),
                            "- json_file(args:{op:\"get_item\", path:string, pointer?:string} | {op:\"query\", path:string, pointer?:string, unique_id?:string, name?:string, resource_type?:string, limit?:int})".to_string(),
                            "- sql_schema / sql_stats / sql_sample / vect_query (discovery context)"
                                .to_string(),
                            "- run_sql (targeted probes)".to_string(),
                            "- artifacts".to_string(),
                        ];
                        tools_card = Self::build_tools_card(
                            "Allowed tools (authoring phase; plan-batched, deterministic):",
                            lines,
                            Vec::new(),
                            Some("Not available in this phase: staging_model, apply_next_cleanse_batch, dbt_validate, publish_dbt_to_provider.".to_string()),
                        );
                    } else if matches!(
                        authoring_policy,
                        crate::data_engineer::authoring_driver::AuthoringToolPolicy::BatchingModelSql
                    ) {
                        let lines = vec![
                            "- apply_next_model_batch(args:{instructions?:string})".to_string(),
                            "- file(args:{op:\"list\"|\"get\", prefix?:string, path?:string, limit?:int, max_chars?:int} | {op:\"patch\", path:string, patch_text:string} | {op:\"rm\", path:string, expected_sha256?:string} | {op:\"mv\", from:string, to:string, expected_sha256?:string})".to_string(),
                            "- json_file(args:{op:\"get_item\", path:string, pointer?:string} | {op:\"query\", path:string, pointer?:string, unique_id?:string, name?:string, resource_type?:string, limit?:int})".to_string(),
                            "- sql_schema / sql_stats / sql_sample / vect_query (discovery context)"
                                .to_string(),
                            "- run_sql (targeted probes)".to_string(),
                            "- artifacts".to_string(),
                        ];
                        tools_card = Self::build_tools_card(
                            "Allowed tools (authoring phase; plan-batched, deterministic):",
                            lines,
                            Vec::new(),
                            Some("Not available in this phase: gold_model (batch tool calls it deterministically), dbt_validate, publish_dbt_to_provider.".to_string()),
                        );
                    } else if matches!(
                        authoring_policy,
                        crate::data_engineer::authoring_driver::AuthoringToolPolicy::BatchingModelSchema
                    ) {
                        let lines = vec![
                            "- apply_next_model_schema_batch(args:{instructions?:string})"
                                .to_string(),
                            "- file(args:{op:\"list\"|\"get\", prefix?:string, path?:string, limit?:int, max_chars?:int} | {op:\"patch\", path:string, patch_text:string} | {op:\"rm\", path:string, expected_sha256?:string} | {op:\"mv\", from:string, to:string, expected_sha256?:string})".to_string(),
                            "- json_file(args:{op:\"get_item\", path:string, pointer?:string} | {op:\"query\", path:string, pointer?:string, unique_id?:string, name?:string, resource_type?:string, limit?:int})".to_string(),
                            "- sql_schema / sql_stats / sql_sample / vect_query (discovery context)"
                                .to_string(),
                            "- run_sql (targeted probes)".to_string(),
                            "- artifacts".to_string(),
                        ];
                        tools_card = Self::build_tools_card(
                            "Allowed tools (authoring phase; plan-batched, deterministic):",
                            lines,
                            Vec::new(),
                            Some("Not available in this phase: gold_model, apply_next_model_batch, dbt_validate, publish_dbt_to_provider.".to_string()),
                        );
                    } else {
                        let mut lines = vec![
                            "- sql_schema(args:{table?:string})".to_string(),
                            "- vect_query(args:{scope:\"dataset\"|\"field\"|\"doc\"|\"artifact\"|\"metric\"|\"model\", query_text:string, k:int})".to_string(),
                            "  - IMPORTANT: arg key is query_text (NOT query). scope must be one of the listed strings (NOT \"table\").".to_string(),
                            "- sql_stats(args:{table:string, field:string}) (requires field; no table-only mode)".to_string(),
                            "- sql_sample(args:{table:string, field:string, k:int}) (top values for a FIELD; not a row sampler)".to_string(),
                            "- run_sql(args:{sql:string}) (use this to sample rows: SELECT * FROM <table> LIMIT 20)".to_string(),
                        ];
                        if phase == control_flow::Phase::CleanseAuthor {
                            lines.push("- staging_model(args:{dataset_ids:[string], instructions?:string, sql?:string|staging_model?:string|expression?:string})".to_string());
                            lines.push("  - IMPORTANT: you MUST provide dataset_ids. This tool will NOT default to all datasets.".to_string());
                            lines.push(
                                "- apply_next_cleanse_schema_batch(args:{instructions?:string})"
                                    .to_string(),
                            );
                        } else {
                            lines.push("- gold_model(args:{items:[{name:string, folder?:\"marts\"|\"core\", goal?:string, description?:string, inputs:[string], instructions?:string}]})".to_string());
                            lines.push("  - IMPORTANT: max 5 items per call. Gold MUST use ref('stg_*') only; NO source().".to_string());
                            lines.push(
                                "- apply_next_model_schema_batch(args:{instructions?:string})"
                                    .to_string(),
                            );
                        }
                        lines.extend_from_slice(&[
                            "- file(args:{op:\"list\"|\"get\", prefix?:string, path?:string, limit?:int, max_chars?:int} | {op:\"patch\", path:string, patch_text:string} | {op:\"rm\", path:string, expected_sha256?:string} | {op:\"mv\", from:string, to:string, expected_sha256?:string})".to_string(),
                            "- json_file(args:{op:\"get_item\", path:string, pointer?:string} | {op:\"query\", path:string, pointer?:string, unique_id?:string, name?:string, resource_type?:string, limit?:int})".to_string(),
                        ]);
                        tools_card = Self::build_tools_card(
                            "Allowed tools (authoring phase):",
                            lines,
                            Vec::new(),
                            Some("Not available in this phase: dbt_validate, publish_dbt_to_provider (suite handles these deterministically).".to_string()),
                        );
                    }
                }
            }
            control_flow::Phase::CleanseReview
            | control_flow::Phase::ModelReview
            | control_flow::Phase::PostPublishReview => {
                // Review phases: keep read-only; do not allow arbitrary SQL execution.
                struct ReadOnlyFilesTool {
                    inner: FilesTool,
                }
                #[async_trait::async_trait]
                impl react_core::tools::Tool for ReadOnlyFilesTool {
                    fn name(&self) -> &'static str {
                        "file"
                    }
                    async fn call(
                        &self,
                        args: serde_json::Value,
                        ctx: &react_core::agent::AgentCtx,
                    ) -> Result<serde_json::Value, String> {
                        if !crate::data_engineer::tool_ops::is_file_read_op(&args) {
                            return Err("file is read-only in review phases; use op='get' or op='list' (mutating ops are disabled: patch/rm/mv)".to_string());
                        }
                        self.inner.call(args, ctx).await
                    }
                }
                reg.register(ReadOnlyFilesTool {
                    inner: FilesTool {
                        datasets: sctx.datasets.clone(),
                    },
                });
                reg.register(JsonFileTool);

                tools_card = Self::build_tools_card(
                    "Allowed tools (review phase, read-only):",
                    vec![
                        "- file (list/get)".to_string(),
                        "- json_file (get_item/query)".to_string(),
                        "- artifacts".to_string(),
                        "- sql_schema / sql_stats / sql_sample / vect_query (read-only context)"
                            .to_string(),
                    ],
                    Vec::new(),
                    Some("Not available: run_sql, staging_model, approve_and_save_artifact(_batch), dbt_validate, publish_dbt_to_provider.".to_string()),
                );
            }
            _ => {
                // Other phases do not run an LLM action set (suite does deterministic steps).
                tools_card =
                    "Allowed tools: (suite deterministic step; no agent tools)".to_string();
            }
        }

        Ok((reg, tools_card))
    }
}
