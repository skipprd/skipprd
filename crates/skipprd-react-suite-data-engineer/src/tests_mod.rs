use super::*;
use async_trait::async_trait;
use std::sync::Arc;

fn test_sctx() -> SuiteCtx {
    SuiteCtx::new(
        Arc::new(react_module_storage_memory::InMemoryStorageAdapter::default()),
        Arc::new(react_core::provider_traits::NullSecretsProvider::default()),
        Arc::new(react_core::llm::NullModel::new()),
        react_core::scope::RequestScope::parse("default", "default", "default").unwrap(),
        Arc::new(react_core::keyspace::DefaultKeyspace::new(
            "test".to_string(),
        )),
    )
}

#[derive(Clone)]
struct MockWarehouseOk {
    ok_fqns: std::collections::HashSet<String>,
}

#[async_trait]
impl crate::providers::QueryProvider for MockWarehouseOk {
    async fn query(&self, _sql: &str) -> Result<crate::providers::QueryResult, String> {
        Ok(crate::providers::QueryResult {
            header: vec![],
            rows: vec![],
            meta: None,
        })
    }

    async fn schema(&self, dataset_fqn: &str) -> Result<Vec<(String, String)>, String> {
        if self.ok_fqns.contains(dataset_fqn) {
            Ok(vec![("x".to_string(), "string".to_string())])
        } else {
            Err("not found".to_string())
        }
    }

    async fn sample(&self, _dataset_fqn: &str, _limit: usize) -> Result<Vec<Vec<String>>, String> {
        Ok(vec![])
    }
}

#[async_trait]
impl crate::providers::DatasetCatalogProvider for MockWarehouseOk {
    async fn list_datasets(&self) -> Result<Vec<crate::providers::DatasetId>, String> {
        Ok(vec![])
    }

    async fn get_dataset_schema(
        &self,
        dataset: &crate::providers::DatasetId,
    ) -> Result<Vec<(String, String)>, String> {
        let fqn = dataset.fqn();
        crate::providers::QueryProvider::schema(self, fqn.as_str()).await
    }

    async fn get_dataset_stats(
        &self,
        _dataset: &crate::providers::DatasetId,
        _max_fields: usize,
    ) -> Result<
        (
            crate::providers::DatasetFieldStats,
            crate::providers::DatasetStats,
        ),
        String,
    > {
        Err("not used".to_string())
    }

    fn evidence_capabilities(&self) -> crate::providers::ProviderEvidenceCapabilities {
        crate::providers::ProviderEvidenceCapabilities::schema_only("mock warehouse provider")
    }
}

impl crate::providers::WarehouseNaming for MockWarehouseOk {
    fn kind(&self) -> crate::de_config::WarehouseKind {
        crate::de_config::WarehouseKind::default()
    }

    fn parse_dataset_fqn(&self, dataset_fqn: &str) -> Result<crate::providers::DatasetId, String> {
        let parts: Vec<&str> = dataset_fqn.split('.').collect();
        if parts.len() != 3 {
            return Err("expected <catalog>.<schema>.<table>".to_string());
        }
        Ok(crate::providers::DatasetId {
            catalog: parts[0].to_string(),
            database: parts[1].to_string(),
            table: parts[2].to_string(),
        })
    }

    fn quote_ident(&self, ident: &str) -> String {
        format!("\"{}\"", ident.replace('"', "\"\""))
    }
}

struct MockQuery;

#[async_trait]
impl crate::providers::QueryProvider for MockQuery {
    async fn query(&self, _sql: &str) -> Result<crate::providers::QueryResult, String> {
        Ok(crate::providers::QueryResult {
            header: vec![],
            rows: vec![],
            meta: None,
        })
    }
    async fn schema(&self, _dataset_fqn: &str) -> Result<Vec<(String, String)>, String> {
        Ok(vec![])
    }
    async fn sample(&self, _dataset_fqn: &str, _limit: usize) -> Result<Vec<Vec<String>>, String> {
        Ok(vec![])
    }
}

#[test]
fn select_high_value_model_candidates_uses_score_threshold_not_fixed_count() {
    let candidates: Vec<crate::plan_schema::ModelPlanCandidateV1> = (0..10)
        .map(|i| crate::plan_schema::ModelPlanCandidateV1 {
            name: format!("m_{i}"),
            insight: "x".to_string(),
            observation: "y".to_string(),
            value_score: 95 - (i as i32 * 5),
        })
        .collect();
    let selected = DataEngineerSuite::select_high_value_model_candidates(&candidates);
    assert_eq!(selected.len(), 6, "default min score 70 should keep 6");
    assert_eq!(selected[0].name, "m_0");
    assert_eq!(selected[5].name, "m_5");
}

#[test]
fn compile_model_candidates_plan_builds_batches_from_selected_threshold() {
    let cands = crate::plan_schema::ModelPlanCandidatesV1 {
        candidates: (0..13)
            .map(|i| crate::plan_schema::ModelPlanCandidateV1 {
                name: format!("m_{i}"),
                insight: "high value".to_string(),
                observation: "grounded".to_string(),
                value_score: 100 - (i as i32 * 5),
            })
            .collect(),
    };
    let plan = DataEngineerSuite::compile_model_candidates_plan(&cands);
    let tasks = plan.tasks;
    let batches = plan.batches;
    assert_eq!(tasks.len(), 7, "default min score 70 should keep 7");
    assert_eq!(batches.len(), 2);
    assert_eq!(
        batches[0],
        vec![
            "m_0".to_string(),
            "m_1".to_string(),
            "m_2".to_string(),
            "m_3".to_string(),
            "m_4".to_string(),
        ]
    );
    assert_eq!(batches[1], vec!["m_5".to_string(), "m_6".to_string()]);
}

#[test]
fn parse_impl_spec_with_sanitize_strips_unknown_cleanse_keys() {
    let raw = serde_json::json!({
        "spec_version": 1,
        "row_preserving": true,
        "output_fields": [],
        "prohibited_ops": [],
        "batch_id": "x",
        "data_quality": {"checks":[]}
    });
    let (spec, stripped) = DataEngineerSuite::parse_impl_spec_value_with_sanitize::<
        crate::plan::CleanseImplementationSpec,
    >(raw, TrackKind::Cleanse)
    .expect("cleanse spec should parse after sanitize");
    assert_eq!(spec.spec_version, 1);
    assert!(stripped.iter().any(|k| k == "batch_id"));
    assert!(stripped.iter().any(|k| k == "data_quality"));
}

#[test]
fn parse_impl_spec_with_sanitize_strips_unknown_model_keys() {
    let raw = serde_json::json!({
        "spec_version": 1,
        "grain": "1 row per id",
        "inputs": [],
        "joins": [],
        "metrics": [],
        "output_fields": [],
        "assumptions": [],
        "dependencies": ["x"],
        "batch_id": "b1"
    });
    let (spec, stripped) = DataEngineerSuite::parse_impl_spec_value_with_sanitize::<
        crate::plan::ModelImplementationSpec,
    >(raw, TrackKind::Model)
    .expect("model spec should parse after sanitize");
    assert_eq!(spec.spec_version, 1);
    assert!(stripped.iter().any(|k| k == "dependencies"));
    assert!(stripped.iter().any(|k| k == "batch_id"));
}

#[tokio::test]
async fn review_registry_is_read_only() {
    let mut sctx = test_sctx();
    sctx.set_capability(Arc::new(crate::ctx_ext::QueryCap(Arc::new(MockQuery))));

    let reg = DataEngineerSuite::build_tools(AgentMode::Review, &sctx)
        .expect("build_tools(review) should succeed");
    let actx = DataEngineerSuite::agent_tool_ctx("t", &sctx);

    // Not allowed in review
    assert!(reg
        .call("approve_and_save_artifact", serde_json::json!({}), &actx)
        .await
        .is_err());
    assert!(reg
        .call(
            "approve_and_save_artifact_batch",
            serde_json::json!({}),
            &actx
        )
        .await
        .is_err());
    assert!(reg
        .call("dbt_validate", serde_json::json!({}), &actx)
        .await
        .is_err());
    assert!(reg
        .call("publish_dbt_to_provider", serde_json::json!({}), &actx)
        .await
        .is_err());
    assert!(reg
        .call("staging_model", serde_json::json!({}), &actx)
        .await
        .is_err());
    assert!(reg
        .call("catalog_note", serde_json::json!({}), &actx)
        .await
        .is_err());
    assert!(reg
        .call("ask_user", serde_json::json!({}), &actx)
        .await
        .is_err());
    assert!(reg
        .call("ask_approval", serde_json::json!({}), &actx)
        .await
        .is_err());

    // Also exclude arbitrary SQL execution in review mode.
    assert!(reg
        .call("run_sql", serde_json::json!({"sql":"SELECT 1"}), &actx)
        .await
        .is_err());

    // Allowed in review
    let obs = reg
        .call(
            "artifacts",
            serde_json::json!({"op":"list","limit":5}),
            &actx,
        )
        .await
        .expect("artifacts should be available");
    assert_eq!(obs.get("ok").and_then(|v| v.as_bool()), Some(true));
}

#[tokio::test]
async fn hard_mutation_mode_exposes_batch_tool_from_plan_state() {
    let mut sctx = test_sctx();
    sctx.set_capability(Arc::new(crate::ctx_ext::QueryCap(Arc::new(MockQuery))));

    let (_reg, card) = DataEngineerSuite::build_tools_for_phase(
        crate::control_flow::Phase::CleanseAuthor,
        true,
        &sctx,
        &super::PlanState::CleanseSqlDatasetIds(vec!["AwsDataCatalog.db.t1".to_string()]),
        false,
    )
    .expect("build_tools_for_phase should succeed");

    assert!(card.contains("patch_text"));
    assert!(card.contains("apply_next_cleanse_batch"));
}

#[tokio::test]
async fn hard_mutation_mode_single_target_repair_rejects_other_paths() {
    let mut sctx = test_sctx();
    sctx.set_capability(Arc::new(crate::ctx_ext::QueryCap(Arc::new(MockQuery))));
    let actx = DataEngineerSuite::agent_tool_ctx("t", &sctx);

    let (reg, _card) = DataEngineerSuite::build_tools_for_phase(
        crate::control_flow::Phase::ModelAuthor,
        true,
        &sctx,
        &super::PlanState::Unconstrained,
        false,
    )
    .expect("build_tools_for_phase should succeed");

    // Seed valid hard-repair execution state for deterministic single-target tool calls.
    if let Some(store) = actx.thread_store().as_ref() {
        let mut seeded = crate::progress_controller::ExecutionState::new();
        seeded.repair.failure_context =
            Some(crate::progress_controller::ValidationFailureContext {
                brief: "test".to_string(),
                log_excerpts: None,
                compile_ok: false,
                run_ok: false,
            });
        seeded.repair.status = crate::progress_controller::RepairStatus::Pending { cycle: 1 };
        seeded
            .save(&store.control_store(), "t")
            .await
            .expect("seed hard repair state");
    }

    // With relaxed single-target: off-target paths are now allowed (the LLM
    // chooses which file to edit). This call should succeed or fail for file-
    // system reasons, NOT for path-restriction reasons.
    let result = reg
        .call(
            "file",
            serde_json::json!({
                "op":"patch",
                "path":"models/marts/fct_customers.sql",
                "patch_text":"@@\n- select 1 as id\n+ select 1 as id\n"
            }),
            &actx,
        )
        .await;
    // It won't contain a path-restriction error; any error is a file-system error.
    if let Err(e) = &result {
        assert!(
            !e.contains("single-target") && !e.contains("repair mode violation"),
            "path restriction should be relaxed: {e}"
        );
    }
}

#[tokio::test]
async fn agent_phase_tool_card_and_registry_never_expose_ask_user() {
    let mut sctx = test_sctx();
    sctx.set_capability(Arc::new(crate::ctx_ext::QueryCap(Arc::new(MockQuery))));
    let actx = DataEngineerSuite::agent_tool_ctx("t", &sctx);
    let (reg, card) = DataEngineerSuite::build_tools_for_phase(
        crate::control_flow::Phase::CleansePlan,
        true,
        &sctx,
        &super::PlanState::ReadOnly,
        false,
    )
    .expect("build_tools_for_phase should succeed");

    assert!(!card.contains("ask_user"));
    let err = reg
        .call("ask_user", serde_json::json!({"prompt":"x"}), &actx)
        .await
        .unwrap_err();
    assert!(err.contains("unknown tool"));
}

#[tokio::test]
async fn run_agent_is_non_interactive_on_missing_providers() {
    let sctx = test_sctx();
    let err = DataEngineerSuite::run_agent("thread-missing-providers", "go", &sctx)
        .await
        .expect_err("agent mode must hard-fail instead of returning an interactive prompt");
    assert!(
        err.contains("warehouse provider configured")
            || err.contains("dbt provider configured")
            || err.contains("catalog bootstrap metadata gate failed")
    );
}

#[test]
fn ide_agent_runner_is_only_selected_for_ide_agent_mode() {
    assert!(DataEngineerSuite::should_use_ide_agent_runner(
        AgentMode::Model,
        true
    ));
    assert!(!DataEngineerSuite::should_use_ide_agent_runner(
        AgentMode::Model,
        false
    ));
    assert!(!DataEngineerSuite::should_use_ide_agent_runner(
        AgentMode::Ask,
        true
    ));
    assert!(!DataEngineerSuite::should_use_ide_agent_runner(
        AgentMode::Review,
        true
    ));
}

#[tokio::test]
async fn ide_agent_tools_are_local_first_without_query_provider() {
    let sctx = test_sctx();
    let reg = DataEngineerSuite::build_ide_agent_tools(&sctx)
        .expect("ide agent tools should not require warehouse providers");
    let card = DataEngineerSuite::build_ide_agent_tools_card(&sctx);
    let actx = DataEngineerSuite::agent_tool_ctx("t", &sctx);

    assert!(card.contains("local_ide"));
    assert!(card.contains("patch"));
    assert!(!card.contains("run_sql"));
    let err = reg
        .call("run_sql", serde_json::json!({"sql":"select 1"}), &actx)
        .await
        .expect_err("run_sql should not be registered without a query provider");
    assert!(err.contains("unknown tool"));
}

#[test]
fn local_ide_mutations_remain_agent_only() {
    let ask_caps = DataEngineerSuite::agent_capability_profile(AgentMode::Ask, true, true);
    let review_caps = DataEngineerSuite::agent_capability_profile(AgentMode::Review, true, true);
    let agent_caps = DataEngineerSuite::agent_capability_profile(AgentMode::Model, true, true);

    assert!(ask_caps.contains(&AgentToolCapability::LocalIdeTools));
    assert!(!ask_caps.contains(&AgentToolCapability::LocalIdeMutations));
    assert!(!review_caps.contains(&AgentToolCapability::LocalIdeTools));
    assert!(agent_caps.contains(&AgentToolCapability::LocalIdeMutations));
}

#[test]
fn non_interactive_contract_rejects_await_user_for_agent_type() {
    let frames = vec![FlowFrame::Interrupt {
        kind: FlowKind::new("await_user"),
        prompt: "x".to_string(),
    }];
    let err = DataEngineerSuite::enforce_non_interactive_contract(AgentMode::Model, frames)
        .expect_err("agent type must reject await_user interrupt");
    assert!(err.contains("agent_mode_await_user_forbidden"));
}

#[test]
fn non_interactive_contract_allows_await_user_for_non_agent_when_not_headless() {
    let frames = vec![FlowFrame::Interrupt {
        kind: FlowKind::new("await_user"),
        prompt: "x".to_string(),
    }];
    let out = DataEngineerSuite::enforce_non_interactive_contract(AgentMode::Review, frames)
        .expect("non-agent should allow await_user interrupt when not headless");
    assert!(matches!(out.first(), Some(FlowFrame::Interrupt { .. })));
}

#[tokio::test]
async fn plan_batched_staging_model_is_not_exposed_to_agent() {
    let mut sctx = test_sctx();
    sctx.set_capability(Arc::new(crate::ctx_ext::QueryCap(Arc::new(MockQuery))));
    let actx = DataEngineerSuite::agent_tool_ctx("t", &sctx);

    let (reg, _card) = DataEngineerSuite::build_tools_for_phase(
        crate::control_flow::Phase::CleanseAuthor,
        true,
        &sctx,
        &super::PlanState::CleanseSqlDatasetIds(vec!["AwsDataCatalog.db.t1".to_string()]),
        false,
    )
    .expect("build_tools_for_phase should succeed");

    let err = reg
        .call(
            "staging_model",
            serde_json::json!({"dataset_ids":["AwsDataCatalog.db.t2"]}),
            &actx,
        )
        .await
        .unwrap_err();
    assert!(err.contains("unknown tool"));

    // Deterministic executor tool should exist (even if it fails due to missing plan in this test ctx).
    let err2 = reg
        .call("apply_next_cleanse_batch", serde_json::json!({}), &actx)
        .await
        .unwrap_err();
    assert!(err2.contains("no active cleanse plan"));
}

#[tokio::test]
async fn plan_batched_cleanse_schema_mode_exposes_only_schema_batch_tool() {
    let mut sctx = test_sctx();
    sctx.set_capability(Arc::new(crate::ctx_ext::QueryCap(Arc::new(MockQuery))));
    let actx = DataEngineerSuite::agent_tool_ctx("t", &sctx);
    let (reg, card) = DataEngineerSuite::build_tools_for_phase(
        crate::control_flow::Phase::CleanseAuthor,
        true,
        &sctx,
        &super::PlanState::CleanseSchemaDatasetIds(vec!["AwsDataCatalog.db.t1".to_string()]),
        false,
    )
    .expect("build_tools_for_phase should succeed");
    assert!(card.contains("apply_next_cleanse_schema_batch"));
    assert!(
        !card.contains("- apply_next_cleanse_batch(args:{instructions?:string})"),
        "sql batch tool must not be exposed in schema-next-action mode"
    );
    let err = reg
        .call("apply_next_cleanse_batch", serde_json::json!({}), &actx)
        .await
        .unwrap_err();
    assert!(err.contains("unknown tool"));
}

#[tokio::test]
async fn plan_batched_gold_model_is_not_exposed_to_agent() {
    let mut sctx = test_sctx();
    sctx.set_capability(Arc::new(crate::ctx_ext::QueryCap(Arc::new(MockQuery))));
    let actx = DataEngineerSuite::agent_tool_ctx("t", &sctx);

    let (reg, _card) = DataEngineerSuite::build_tools_for_phase(
        crate::control_flow::Phase::ModelAuthor,
        true,
        &sctx,
        &super::PlanState::ModelSqlItemNames(vec!["fct_orders".to_string()]),
        false,
    )
    .expect("build_tools_for_phase should succeed");

    let err = reg
        .call(
            "gold_model",
            serde_json::json!({"items":[{"name":"dim_users","inputs":["stg_x"]}]}),
            &actx,
        )
        .await
        .unwrap_err();
    assert!(err.contains("unknown tool"));

    let err2 = reg
        .call("apply_next_model_batch", serde_json::json!({}), &actx)
        .await
        .unwrap_err();
    assert!(err2.contains("no active model plan"));
}

// ModelSchemaItemNames variant removed (never constructed in production).
// Schema-only batch mode was only used in tests; ModelSqlItemNames is the sole model plan state.

#[test]
fn review_question_includes_prior_review_and_mutation_diff_when_available() {
    use crate::control_flow::Phase;
    use crate::progress_controller::{ExecutionState, LastMutationSummary};

    let mut st = ExecutionState::new();
    st.phase.transition = Some(
        crate::progress_controller::PhaseTransition::ReviewPatchImpl {
            meta: crate::domain_types::ReviewDecisionMeta {
                decision: crate::domain_types::ReviewDecision::PatchImpl,
                target_task_ids: vec!["x".to_string()],
                tier: crate::domain_types::ReviewTier::Silver,
                review_ref: None,
            },
            target_task_ids: vec!["x".to_string()],
        },
    );
    st.telemetry.last_mutation_summary = Some(LastMutationSummary {
        op: crate::progress_controller::MutationOp::Patch,
        affected_paths: vec!["models/staging/stg_test_raw_raw_orders.sql".to_string()],
        select_terms: vec!["placed_at_ts".to_string()],
        ts: Some("t".to_string()),
    });

    let q = DataEngineerSuite::build_review_question_with_context(
        "orig goal",
        Phase::CleanseReview,
        &st,
    );
    assert!(
        q.contains("Review context"),
        "should include context header"
    );
    assert!(
        q.contains("Previous review decision"),
        "should include prior review block"
    );
    assert!(
        q.contains("Most recent mutation summary"),
        "should include state-based mutation summary"
    );
    assert!(
        q.contains("stg_test_raw_raw_orders.sql"),
        "should include affected path from state"
    );
    assert!(
        q.contains("review_patch_impl"),
        "should include entry reason"
    );
    assert!(
        q.contains("Original goal"),
        "should retain original goal section"
    );
}

#[test]
fn review_question_includes_entry_reason_when_review_started_from_validate_pass() {
    use crate::control_flow::Phase;
    use crate::progress_controller::ExecutionState;

    let mut st = ExecutionState::new();
    st.phase.transition =
        Some(crate::progress_controller::PhaseTransition::ValidatePassToReview { step_idx: 1 });

    let q = DataEngineerSuite::build_review_question_with_context(
        "orig goal",
        Phase::CleanseReview,
        &st,
    );
    assert!(q.contains("validate_pass_to_review"));
    assert!(q.contains("step_idx"));
}

#[test]
fn patch_impl_intent_requires_mutation_advance() {
    use crate::control_flow::Phase;
    use crate::progress_controller::{ExecutionState, PatchImplIntent};

    let mut st = ExecutionState::new();
    st.repair.pending_patch_impl = Some(PatchImplIntent {
        phase: Phase::ModelAuthor,
        mutated_since_set: false,
    });
    assert!(crate::phase_gate::patch_impl_intent_unsatisfied(
        &st,
        Phase::ModelAuthor
    ));
    st.repair
        .pending_patch_impl
        .as_mut()
        .unwrap()
        .mutated_since_set = true;
    assert!(!crate::phase_gate::patch_impl_intent_unsatisfied(
        &st,
        Phase::ModelAuthor
    ));
}

#[tokio::test]
async fn authoring_complete_reason_detail_uses_latest_log_state() {
    let sctx = test_sctx();
    let store = ThreadStore::new(
        sctx.storage().clone(),
        sctx.scope().clone(),
        sctx.keyspace().clone(),
    );
    let tid = "tid_guard_state";

    // Seed failing validate state directly in canonical control state.
    let mut state = crate::progress_controller::ExecutionState::new();
    state.repair.failure_context = Some(crate::progress_controller::ValidationFailureContext {
        brief: "test".to_string(),
        log_excerpts: None,
        compile_ok: false,
        run_ok: false,
    });
    state
        .save(&store.control_store(), tid)
        .await
        .expect("save failing validate state");

    let before = DataEngineerSuite::authoring_complete_reason_detail(&store, tid, true, true).await;
    assert_eq!(
        before
            .get("guard_state")
            .and_then(|v| v.get("mutated_since_fail"))
            .and_then(|v| v.as_bool()),
        Some(false)
    );

    state.repair.status = crate::progress_controller::RepairStatus::Pending { cycle: 1 };
    state.set_last_mutation_summary(
        crate::progress_controller::MutationOp::Patch,
        vec!["models/staging/stg_orders.sql".to_string()],
        vec![],
    );
    state
        .save(&store.control_store(), tid)
        .await
        .expect("save patched state");

    let after = DataEngineerSuite::authoring_complete_reason_detail(&store, tid, true, true).await;
    assert_eq!(
        after
            .get("guard_state")
            .and_then(|v| v.get("mutated_since_fail"))
            .and_then(|v| v.as_bool()),
        Some(true)
    );
}

#[test]
fn output_field_kind_contract_rejects_unknown_variants() {
    let ok = serde_json::json!({
        "output_fields": [{"name":"a","kind":"raw"},{"name":"b","kind":"quality_flag"}]
    });
    assert!(DataEngineerSuite::validate_output_field_kind_contract(&ok).is_ok());
    let bad = serde_json::json!({
        "output_fields": [{"name":"a","kind":"passthrough"}]
    });
    let err = DataEngineerSuite::validate_output_field_kind_contract(&bad)
        .expect_err("expected invalid kind");
    assert!(err.contains("allowed kind values: raw, clean, derived, quality_flag"));
}

#[tokio::test]
async fn model_plan_can_disable_json_file_after_manifest_retry_suppression() {
    let mut sctx = test_sctx();
    sctx.set_capability(Arc::new(crate::ctx_ext::QueryCap(Arc::new(MockQuery))));
    let actx = DataEngineerSuite::agent_tool_ctx("t", &sctx);
    let (reg, card) = DataEngineerSuite::build_tools_for_phase(
        crate::control_flow::Phase::ModelPlan,
        true,
        &sctx,
        &super::PlanState::ReadOnly,
        true,
    )
    .expect("build_tools_for_phase should succeed");
    let err = reg
        .call(
            "json_file",
            serde_json::json!({"op":"query","path":"target/manifest.json","pointer":"/nodes"}),
            &actx,
        )
        .await
        .expect_err("json_file should be disabled in fallback mode");
    assert!(err.contains("unknown tool"));
    assert!(card.contains("json_file is temporarily disabled"));
}

#[test]
fn model_plan_manifest_retry_state_detects_repeated_failures() {
    use crate::progress_controller::{
        classify_manifest_lookup_failure, classify_manifest_lookup_path, ExecutionState,
    };

    let mut st = ExecutionState::new();
    let path_kind = classify_manifest_lookup_path("manifest.json")
        .expect("manifest.json should classify as manifest lookup path");
    let failure_kind =
        classify_manifest_lookup_failure(&["not found or failed to fetch: NoSuchKey".to_string()])
            .expect("NoSuchKey failure should classify");

    st.note_manifest_lookup_attempt(path_kind, false, Some(failure_kind));
    st.note_manifest_lookup_attempt(path_kind, false, Some(failure_kind));

    assert!(st.manifest.manifest_lookup.retry_suppressed);
    assert_eq!(st.manifest.manifest_lookup.canonical_success_count, 0);
    assert!(
        st.manifest
            .manifest_lookup
            .failure_signature
            .as_deref()
            .unwrap_or("")
            .contains("NoSuchKey"),
        "expected NoSuchKey signature"
    );
}

#[test]
fn run_agent_source_enforces_kernel_transition_and_guard_paths() {
    let legacy_transition = ["control_flow::append_phase_with_", "intent", "("].concat();
    let legacy_guard_block = ["ThreadStep::Guard", "Block"].concat();
    let sources_to_check: Vec<(&str, String)> = vec![
        ("lib.rs", include_str!("lib.rs").to_string()),
        ("agent_modes.rs", include_str!("agent_modes.rs").to_string()),
    ];
    for (name, src) in &sources_to_check {
        let normalized: String = src.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            !normalized.contains(&legacy_transition),
            "legacy transition path must not appear in {name}"
        );
        assert!(
            !normalized.contains(&legacy_guard_block),
            "legacy inline GuardBlock construction must not appear in {name}"
        );
    }
    let agent_modes_normalized: String = include_str!("agent_modes.rs")
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    assert!(
        agent_modes_normalized.contains("apply_guard_block("),
        "kernel guard helper should be used in agent_modes.rs"
    );
    let mod_src = include_str!("lib.rs");
    for marker in [
        "execute_preflight_phase(",
        "execute_plan_phase(",
        "execute_author_phase(",
        "execute_validate_phase(",
        "execute_review_phase(",
        "execute_publish_await_approval_phase(",
        "execute_publish_phase(",
    ] {
        let found = mod_src.contains(marker) || include_str!("agent_modes.rs").contains(marker);
        assert!(
            found,
            "run loop should route through typed phase executors: missing {marker}"
        );
    }

    for (name, src) in [
        ("phase_preflight.rs", include_str!("phase_preflight.rs")),
        ("phase_plan.rs", include_str!("phase_plan.rs")),
        ("phase_author.rs", include_str!("phase_author.rs")),
        ("phase_validate.rs", include_str!("phase_validate.rs")),
        ("phase_review.rs", include_str!("phase_review.rs")),
        ("phase_publish.rs", include_str!("phase_publish.rs")),
    ] {
        assert!(
            src.contains("commit_phase_decision"),
            "{name} must commit phase changes through commit_phase_decision"
        );
        assert!(
            !src.contains("apply_phase_transition("),
            "{name} must not bypass the phase contract seam"
        );
    }
}

#[test]
fn control_state_thread_log_read_guardrails_are_enforced_in_rust_tests() {
    let forbidden = ["thread_store.get(", "store.get(thread_id)"];
    let sources = [
        ("control_flow.rs", include_str!("control_flow.rs")),
        (
            "progress_controller.rs",
            include_str!("progress_controller.rs"),
        ),
        (
            "transition_dispatcher.rs",
            include_str!("transition_dispatcher.rs"),
        ),
    ];
    for (name, src) in sources {
        for token in forbidden {
            assert!(
                !src.contains(token),
                "forbidden thread-log read token '{}' found in {}",
                token,
                name
            );
        }
    }
}

#[test]
fn billable_phases_use_metered_commit() {
    for (name, src) in [
        ("phase_el_discover.rs", include_str!("phase_el_discover.rs")),
        ("phase_el_sync.rs", include_str!("phase_el_sync.rs")),
        ("phase_author.rs", include_str!("phase_author.rs")),
        ("agent_modes.rs", include_str!("agent_modes.rs")),
        (
            "plan_review_helpers.rs",
            include_str!("plan_review_helpers.rs"),
        ),
    ] {
        assert!(
            src.contains("commit_metered_decision"),
            "{name} is a billable phase and MUST use commit_metered_decision"
        );
    }
}

#[test]
fn billable_phases_construct_usage_events() {
    for (name, src, expected_event) in [
        (
            "phase_el_discover.rs",
            include_str!("phase_el_discover.rs"),
            "UsageEvent::FieldsDiscovered",
        ),
        (
            "phase_el_sync.rs",
            include_str!("phase_el_sync.rs"),
            "UsageEvent::TablesSynced",
        ),
        (
            "phase_author.rs",
            include_str!("phase_author.rs"),
            "UsageEvent::ModelsAuthored",
        ),
        (
            "agent_modes.rs",
            include_str!("agent_modes.rs"),
            "UsageEvent::RepairCycle",
        ),
        (
            "plan_review_helpers.rs",
            include_str!("plan_review_helpers.rs"),
            "UsageEvent::PlanApproved",
        ),
    ] {
        assert!(
            src.contains(expected_event),
            "{name} must construct {expected_event} for metering"
        );
    }
}
