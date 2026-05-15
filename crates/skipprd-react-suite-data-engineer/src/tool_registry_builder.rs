use super::tool_policies::*;
use super::*;

/// LLM-facing tool registries always wrap the base `FilesTool` with `PolicyFilesTool`. The deny-
/// list policy consults `crate::file_ownership` so a single failed write already teaches the LLM
/// where to put it. Deterministic suite-internal callers (e.g. `invariant_has_dbt_project`) use
/// the base `FilesTool` directly because they are not user-controllable surfaces.
impl DataEngineerSuite {
    pub(super) fn build_tools(
        agent_mode: AgentMode,
        sctx: &SuiteCtx,
    ) -> Result<ToolRegistry, String> {
        use crate::tools::{
            artifacts::ArtifactsTool, files_tool::FilesTool, sql_run::SqlRunTool,
            sql_sample::SqlSampleTool, sql_schema::SqlSchemaTool, sql_stats::SqlStatsTool,
            vect_query::VectQueryTool,
        };

        let mut registry = ToolRegistry::new();

        let query =
            crate::ctx_ext::sctx_query(sctx).ok_or_else(|| "query provider missing".to_string())?;

        registry.register(SqlSchemaTool {
            query: query.clone(),
            datasets: crate::ctx_ext::sctx_datasets(sctx),
            catalog: crate::ctx_ext::sctx_catalog(sctx),
        });
        registry.register(SqlStatsTool {
            catalog: crate::ctx_ext::sctx_catalog(sctx),
            datasets: crate::ctx_ext::sctx_datasets(sctx),
        });
        registry.register(SqlSampleTool {
            query: query.clone(),
        });
        registry.register(VectQueryTool);

        let allow_user_interrupt_tools = !Self::headless_mode_enabled();
        let caps = Self::agent_capability_profile(
            agent_mode,
            allow_user_interrupt_tools,
            Self::ide_chat_surface_enabled(),
        );

        if caps.contains(&AgentToolCapability::ReadOnlyFile) {
            registry.register(PolicyFilesTool {
                inner: FilesTool {
                    datasets: crate::ctx_ext::sctx_datasets(sctx),
                },
                policy: FileAccessPolicy::ReadOnly {
                    error_message: "file is read-only in this mode; use op='get' or op='list' (mutating ops are disabled: patch/rm/mv)",
                },
            });
        } else if caps.contains(&AgentToolCapability::MutableFile) {
            registry.register(PolicyFilesTool {
                inner: FilesTool {
                    datasets: crate::ctx_ext::sctx_datasets(sctx),
                },
                policy: FileAccessPolicy::SystemOwnedDenyList,
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
                datasets: crate::ctx_ext::sctx_datasets(sctx),
            });
        }
        if caps.contains(&AgentToolCapability::GoldModel) {
            registry.register(tools::gold_model::GoldModelTool);
        }
        if caps.contains(&AgentToolCapability::DbtValidate) {
            registry.register(ThreadDerivedDbtValidateTool {
                inner: tools::dbt_validate::DbtValidateTool {
                    datasets: crate::ctx_ext::sctx_datasets(sctx),
                    catalog: crate::ctx_ext::sctx_catalog(sctx),
                },
            });
        }
        if caps.contains(&AgentToolCapability::PublishDbt) {
            registry.register(tools::publish_dbt_to_provider::PublishDbtToProviderTool {
                datasets: crate::ctx_ext::sctx_datasets(sctx),
                catalog: crate::ctx_ext::sctx_catalog(sctx),
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
        if caps.contains(&AgentToolCapability::SkipprCli) {
            registry.register(tools::skippr_cli::SkipprCliTool);
        }
        if caps.contains(&AgentToolCapability::LocalIdeTools) {
            registry.register(tools::local_ide::LocalIdeTool {
                allow_patch: caps.contains(&AgentToolCapability::LocalIdeMutations),
            });
        }

        Ok(registry)
    }

    pub(super) fn build_ide_agent_tools(sctx: &SuiteCtx) -> Result<ToolRegistry, String> {
        use crate::tools::{
            artifacts::ArtifactsTool, sql_run::SqlRunTool, vect_query::VectQueryTool,
        };

        let mut registry = ToolRegistry::new();
        let caps = Self::agent_capability_profile(AgentMode::Agent, true, true);

        registry.register(VectQueryTool);
        if let Some(query) = crate::ctx_ext::sctx_query(sctx) {
            registry.register(SqlRunTool { query });
        }
        if caps.contains(&AgentToolCapability::AskApproval) {
            registry.register(tools::ask_approval::AskApprovalTool);
        }
        if caps.contains(&AgentToolCapability::Artifacts) {
            registry.register(ArtifactsTool);
        }
        registry.register(tools::skippr_cli::SkipprCliTool);
        if caps.contains(&AgentToolCapability::LocalIdeTools) {
            registry.register(tools::local_ide::LocalIdeTool {
                allow_patch: caps.contains(&AgentToolCapability::LocalIdeMutations),
            });
        }

        Ok(registry)
    }

    pub(super) fn build_ide_agent_tools_card(sctx: &SuiteCtx) -> String {
        let mut lines = vec![
            "- local_ide(args:{op:\"list\"|\"read\"|\"grep\"|\"head\"|\"tail\"|\"patch\", path?:string, pattern?:string, patch_text?:string, limit?:int, max_chars?:int}) for bounded local IDE file/search/patch work. For explicit local file edits, use local_ide read -> patch directly and skip vector lookup.".to_string(),
            "- local_ide patch_text must be hunks-only Cursor/Aider format, e.g. {\"op\":\"patch\",\"path\":\"src/app.ts\",\"patch_text\":\"@@ ... @@\\n- old\\n+ new\\n\"}; never include *** Begin Patch envelopes, *** Update File headers, diff --git, or ---/+++ file headers.".to_string(),
            "- ask_approval(args:{prompt:string}) for user consent before workflow escalation only; do not use routine approval before explicit local file edits because the IDE inline diff review is the approval surface.".to_string(),
            "- vect_query(args:{scope?:string, query_text:string, k?:int}) for docs/artifact lookup when local file evidence is not enough".to_string(),
            "- skippr_cli(args:{command:\"user\"|\"doctor\"|\"test\"|\"connect\", action?:string, pipeline?:string, select?:string[]})".to_string(),
            "- artifacts".to_string(),
        ];
        if crate::ctx_ext::sctx_query(sctx).is_some() {
            lines.push("- run_sql(args:{sql:string}) for explicit warehouse questions".to_string());
        }
        Self::build_tools_card("Allowed tools (IDE agent mode):", lines, Vec::new(), None)
    }

    pub(super) fn build_tools_card_for_agent_type(agent_mode: AgentMode) -> String {
        let allow_user_interrupt_tools = !Self::headless_mode_enabled();
        let caps = Self::agent_capability_profile(
            agent_mode,
            allow_user_interrupt_tools,
            Self::ide_chat_surface_enabled(),
        );
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
                let mut lines = vec!["- file(args:{op:\"list\"|\"get\", prefix?:string, path?:string, limit?:int, max_chars?:int})".to_string(),
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
                if caps.contains(&AgentToolCapability::SkipprCli) {
                    lines.push("- skippr_cli(args:{command:\"user\"|\"doctor\"|\"test\"|\"connect\", action?:string, pipeline?:string, select?:string[]})".to_string());
                }
                if caps.contains(&AgentToolCapability::LocalIdeTools) {
                    lines.push("- local_ide(args:{op:\"list\"|\"read\"|\"grep\"|\"head\"|\"tail\", path?:string, pattern?:string, limit?:int, max_chars?:int}) for bounded read-only local IDE file/search work".to_string());
                }
                if caps.contains(&AgentToolCapability::AskUser) {
                    lines.push("- ask_user(args:{prompt:string})".to_string());
                }
                Self::build_tools_card("Allowed tools (ask mode):", lines, Vec::new(), None)
            }
            AgentMode::Agent => {
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
                if caps.contains(&AgentToolCapability::LocalIdeTools) {
                    lines.push("- local_ide(args:{op:\"list\"|\"read\"|\"grep\"|\"head\"|\"tail\"|\"patch\", path?:string, pattern?:string, patch_text?:string, limit?:int, max_chars?:int}) for bounded local IDE file/search/patch work. For explicit local file edits, use local_ide read -> patch directly and skip vector lookup.".to_string());
                    lines.push("- local_ide patch_text must be hunks-only Cursor/Aider format, e.g. {\"op\":\"patch\",\"path\":\"src/app.ts\",\"patch_text\":\"@@ ... @@\\n- old\\n+ new\\n\"}; never include *** Begin Patch envelopes, *** Update File headers, diff --git, or ---/+++ file headers.".to_string());
                }
                Self::build_tools_card("Allowed tools (agent mode):", lines, Vec::new(), None)
            }
        }
    }

    pub(super) fn build_tools_for_phase(
        phase: control_flow::Phase,
        _allow_ask_approval: bool,
        sctx: &SuiteCtx,
        plan_state: &PlanState,
        suppress_manifest_json_in_plan: bool,
    ) -> Result<(ToolRegistry, String), String> {
        use crate::tools::{
            artifacts::ArtifactsTool, files_tool::FilesTool, json_file::JsonFileTool,
            sql_run::SqlRunTool, sql_sample::SqlSampleTool, sql_schema::SqlSchemaTool,
            sql_stats::SqlStatsTool, vect_query::VectQueryTool,
        };

        let query =
            crate::ctx_ext::sctx_query(sctx).ok_or_else(|| "query provider missing".to_string())?;
        let datasets_opt = crate::ctx_ext::sctx_datasets(sctx);

        let mut reg = ToolRegistry::new();

        // Common read tools (safe in most phases)
        reg.register(SqlSchemaTool {
            query: query.clone(),
            datasets: crate::ctx_ext::sctx_datasets(sctx),
            catalog: crate::ctx_ext::sctx_catalog(sctx),
        });
        reg.register(SqlStatsTool {
            catalog: crate::ctx_ext::sctx_catalog(sctx),
            datasets: crate::ctx_ext::sctx_datasets(sctx),
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

                reg.register(PolicyFilesTool {
                    inner: FilesTool {
                        datasets: crate::ctx_ext::sctx_datasets(sctx),
                    },
                    policy: FileAccessPolicy::ReadOnly {
                        error_message: "file is read-only in plan phases; use op='get' or op='list' (mutating ops are disabled: patch/rm/mv)",
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
                let batch_tool_name =
                    register_batch_tool_for_plan_state(&mut reg, phase, plan_state, &datasets_opt);

                reg.register(SqlRunTool {
                    query: query.clone(),
                });
                reg.register(tools::dbt_examples::SearchDbtExamplesTool);
                reg.register(JsonFileTool);

                match plan_state {
                    PlanState::Unconstrained => {
                        reg.register(PolicyFilesTool {
                            inner: FilesTool {
                                datasets: crate::ctx_ext::sctx_datasets(sctx),
                            },
                            policy: FileAccessPolicy::SystemOwnedDenyList,
                        });
                        if phase == control_flow::Phase::CleanseAuthor {
                            reg.register(tools::staging_model::StagingModelTool {
                                datasets: crate::ctx_ext::sctx_datasets(sctx),
                            });
                            reg.register(
                                tools::apply_next_schema_batch::ApplyNextCleanseSchemaBatchTool {
                                    datasets: crate::ctx_ext::sctx_datasets(sctx),
                                },
                            );
                        } else {
                            reg.register(tools::gold_model::GoldModelTool);
                            reg.register(
                                tools::apply_next_schema_batch::ApplyNextModelSchemaBatchTool {
                                    datasets: crate::ctx_ext::sctx_datasets(sctx),
                                },
                            );
                        }
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
                            lines.push(format!("  - IMPORTANT: max {} items per call. Gold uses ref() for inputs (stg_* or intra-plan gold models); NO source().", crate::plan_progress::MAX_BATCH_SIZE));
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
                    PlanState::Repair => {
                        reg.register(PolicyFilesTool {
                            inner: FilesTool {
                                datasets: crate::ctx_ext::sctx_datasets(sctx),
                            },
                            policy: FileAccessPolicy::SystemOwnedDenyList,
                        });
                        let lines = vec![
                            "- file(args:{op:\"list\"|\"get\"|\"patch\"|\"write\"|\"rm\"|\"mv\", ...})".to_string(),
                            "- json_file(args:{op:\"get_item\"|\"query\", ...})".to_string(),
                            "- sql_schema / sql_stats / sql_sample / vect_query (discovery)".to_string(),
                            "- run_sql (targeted probes)".to_string(),
                            "- artifacts".to_string(),
                        ];
                        tools_card = Self::build_tools_card(
                            "Allowed tools (repair; targeted fixes only):",
                            lines,
                            Vec::new(),
                            Some("Not available: gold_model, staging_model, apply_next_*_batch, dbt_validate, publish_dbt_to_provider. Use file(op:\"patch\") for targeted SQL/YAML fixes.".to_string()),
                        );
                    }
                    PlanState::ReadOnly => {
                        reg.register(PolicyFilesTool {
                            inner: FilesTool {
                                datasets: crate::ctx_ext::sctx_datasets(sctx),
                            },
                            policy: FileAccessPolicy::ReadOnly {
                                error_message:
                                    "file is read-only in this context; use op='get' or op='list'",
                            },
                        });
                        tools_card = Self::build_tools_card(
                            "Allowed tools (read-only):",
                            vec![
                                "- file (list/get)".to_string(),
                                "- json_file (get_item/query)".to_string(),
                                "- sql_schema / sql_stats / sql_sample / vect_query (discovery)".to_string(),
                                "- run_sql (targeted probes)".to_string(),
                                "- artifacts".to_string(),
                            ],
                            Vec::new(),
                            Some("Not available: staging_model, gold_model, apply_next_*_batch, file patch/rm/mv, dbt_validate, publish_dbt_to_provider.".to_string()),
                        );
                    }
                    PlanState::CleanseSqlDatasetIds(_)
                    | PlanState::CleanseSchemaDatasetIds(_)
                    | PlanState::ModelSqlItemNames(_) => {
                        reg.register(PolicyFilesTool {
                            inner: FilesTool {
                                datasets: crate::ctx_ext::sctx_datasets(sctx),
                            },
                            policy: FileAccessPolicy::SystemOwnedDenyList,
                        });
                        let batch_name = batch_tool_name.unwrap_or("apply_next_batch");
                        let lines = vec![
                            format!("- {batch_name}(args:{{instructions?:string}})"),
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
                            Some("Not available in this phase: dbt_validate, publish_dbt_to_provider.".to_string()),
                        );
                    }
                }
            }
            control_flow::Phase::CleanseReview | control_flow::Phase::ModelReview => {
                // Review phases: keep read-only; do not allow arbitrary SQL execution.
                reg.register(PolicyFilesTool {
                    inner: FilesTool {
                        datasets: crate::ctx_ext::sctx_datasets(sctx),
                    },
                    policy: FileAccessPolicy::ReadOnly {
                        error_message: "file is read-only in review phases; use op='get' or op='list' (mutating ops are disabled: patch/rm/mv)",
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
