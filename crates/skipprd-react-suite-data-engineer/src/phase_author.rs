use super::*;
use crate::control_flow::Phase;
use react_core::storage::{retry_get_bytes, retry_list_prefix};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AuthorValidateTrigger {
    WorkGroupValidate,
    PlanTasksDone,
}

enum AuthorPlanLoadResult {
    EarlyReturn(PhaseOutcome),
    Ready {
        plan_context: String,
        plan_state: PlanState,
    },
}

struct AuthorPrompt {
    question: String,
    llm_options: LlmCallOptions,
}

enum AuthorEscalation {
    PlanDefect {
        violations: Vec<crate::progress_controller::PlanViolation>,
        strategy: crate::progress_controller::PlanRevisionStrategy,
    },
    FatalLocal(String),
}

async fn apply_author_escalation(
    thread_store: &ThreadStore,
    thread_id: &str,
    phase: Phase,
    escalation: AuthorEscalation,
) -> Result<PhaseOutcome, PhaseError> {
    match escalation {
        AuthorEscalation::PlanDefect {
            violations,
            strategy,
        } => {
            crate::phase_contract::commit_plan_revision_loopback(
                thread_store,
                thread_id,
                phase,
                violations,
                strategy,
            )
            .await?;
            Ok(PhaseOutcome::TransitionCommitted)
        }
        AuthorEscalation::FatalLocal(reason) => Err(PhaseError::Fatal(reason)),
    }
}

struct AuthorPhaseCtx<'a> {
    thread_store: &'a ThreadStore,
    thread_id: &'a str,
    phase: Phase,
    track: TrackKind,
    execution_state: &'a crate::progress_controller::ExecutionState,
    repair_ctx: &'a crate::progress_controller::RepairContext,
    last_validate_failed: bool,
    _mutated_since_fail: bool,
    probe_required: bool,
    probe_satisfied: bool,
}

fn decide_author_validate_trigger(
    next_action: &crate::plan::AuthoringNextAction,
    completion_snapshot: &crate::plan::PlanCompletionSnapshot,
    last_validate_failed: bool,
) -> Option<AuthorValidateTrigger> {
    if last_validate_failed {
        return None;
    }
    if matches!(next_action, crate::plan::AuthoringNextAction::Validate) {
        return Some(AuthorValidateTrigger::WorkGroupValidate);
    }
    if completion_snapshot.completion_state() == crate::plan::PlanCompletionState::Complete {
        return Some(AuthorValidateTrigger::PlanTasksDone);
    }
    None
}

fn review_patch_detail(
    execution_state: &crate::progress_controller::ExecutionState,
) -> Option<crate::phase_reason_detail::ReviewDecisionTransitionDetail> {
    match execution_state.phase.transition.as_ref()? {
        crate::progress_controller::PhaseTransition::ReviewPatchImpl {
            meta,
            target_task_ids: _,
        } => Some(crate::phase_reason_detail::ReviewDecisionTransitionDetail {
            meta: meta.clone(),
            review_phase: String::new(),
            answer: crate::domain_types::ReviewDecision::PatchImpl,
            forced_progress_guard: false,
            forced_progress_by_subjective_retry: false,
            review_subjective_retry_count: 0,
            trigger_step_idx: 0,
            trigger_step: serde_json::Value::Null,
        }),
        _ => None,
    }
}

fn normalize_review_patch_target_path(path: &str) -> String {
    path.trim().trim_start_matches("./").to_string()
}

fn path_matches_review_target(expected_path: Option<&str>, target: &str) -> bool {
    let Some(expected_path) = expected_path else {
        return false;
    };
    let expected_norm = normalize_review_patch_target_path(expected_path);
    let target_norm = normalize_review_patch_target_path(target);
    if expected_norm == target_norm {
        return true;
    }
    let expected_file = std::path::Path::new(&expected_norm);
    let target_file = std::path::Path::new(&target_norm);
    let expected_name = expected_file.file_name().and_then(|s| s.to_str());
    let target_name = target_file.file_name().and_then(|s| s.to_str());
    if expected_name == target_name {
        return true;
    }
    let expected_stem = expected_file.file_stem().and_then(|s| s.to_str());
    let target_stem = target_file.file_stem().and_then(|s| s.to_str());
    expected_stem.is_some() && expected_stem == target_stem
}

fn resolve_review_patch_target_paths<T>(
    target_task_ids: &[String],
    tasks: &[T],
    task_id_of: impl Fn(&T) -> &str,
    expected_path_of: impl Fn(&T) -> Option<&str>,
) -> Vec<String> {
    let mut paths = std::collections::BTreeSet::new();
    for target_task_id in target_task_ids {
        let target = target_task_id.trim();
        if target.is_empty() {
            continue;
        }
        if target.contains('/')
            || target.ends_with(".sql")
            || target.ends_with(".yml")
            || target.ends_with(".yaml")
        {
            paths.insert(normalize_review_patch_target_path(target));
            continue;
        }
        for task in tasks {
            if task_id_of(task) == target
                || path_matches_review_target(expected_path_of(task), target)
            {
                if let Some(path) = expected_path_of(task) {
                    paths.insert(normalize_review_patch_target_path(path));
                }
            }
        }
    }
    paths.into_iter().collect()
}

fn build_post_validate_fail_repair_context(
    track: TrackKind,
    plan_key: &str,
    repair_ctx: &crate::progress_controller::RepairContext,
) -> String {
    let mut ctx = format!(
        "Approved {} plan (stored at: {}).\n\
         All plan tasks were previously completed, but the last validation FAILED.\n\
         You MUST diagnose and fix the failing model(s) before validation can pass.\n",
        track.as_str(),
        plan_key,
    );
    if let Some(brief) = repair_ctx.brief() {
        ctx.push_str(&format!("\n## Validation Error Summary\n{brief}\n"));
    }
    if let Some(excerpts) = repair_ctx.log_excerpts() {
        ctx.push_str(&format!("\n## Relevant Log Lines\n{excerpts}\n"));
    }
    ctx.push_str(
        "\nNext action: read the failing file(s), identify the root cause from the errors above, \
         and apply a targeted fix using file ops. Focus on column mismatches, missing refs, and SQL errors.\n",
    );
    ctx
}

fn build_review_patch_plan_context(
    track: TrackKind,
    plan_key: &str,
    review_target_paths: &[String],
) -> String {
    let mut ctx = format!(
        "Approved {} plan (stored at: {}).\nReview requested a localized implementation patch. Do NOT revise the approved plan/spec.\nYou MUST make a mutating file edit before any further validate turn.\n",
        track.as_str(),
        plan_key
    );
    if review_target_paths.is_empty() {
        ctx.push_str(
            "Review targets were not mapped to file paths. Use the prior review feedback plus the approved plan to identify the relevant implementation and patch it directly.\n",
        );
    } else {
        ctx.push_str(&format!(
            "\nReviewed implementation targets (patch these files directly with file {}):\n",
            crate::tool_ops::general_mutation_ops_label(),
        ));
        for path in review_target_paths {
            ctx.push_str("- ");
            ctx.push_str(path);
            ctx.push('\n');
        }
    }
    ctx.push_str(
        "\nNext action: apply the smallest implementation fix that satisfies the prior review feedback. Keep changes local and preserve the approved grounded plan.\n",
    );
    ctx.push_str(
        "If the feedback cannot be satisfied without changing the approved plan/spec, do NOT edit the plan in this PatchImpl loop. Return a concise complete result beginning with PLAN_CHANGE_REQUIRED and include the exact contradiction, target task, and file path.\n",
    );
    ctx
}

async fn append_review_patch_target_contents(
    q: &mut String,
    actx: &AgentCtx,
    review_target_paths: &[String],
) {
    if review_target_paths.is_empty() {
        return;
    }
    let base = actx
        .keyspace()
        .scoped_prefix(actx.scope(), &["dbt"])
        .trim_end_matches('/')
        .to_string();
    let mut rendered = 0usize;
    for path in review_target_paths {
        if rendered >= 3 {
            break;
        }
        let rel = normalize_review_patch_target_path(path);
        if rel.is_empty() {
            continue;
        }
        let key = format!("{}/{}", base, rel);
        let Ok(bytes) = retry_get_bytes(actx.storage().as_ref(), &key).await else {
            continue;
        };
        let content = String::from_utf8_lossy(&bytes).to_string();
        let fence_lang = if rel.ends_with(".yml") || rel.ends_with(".yaml") {
            "yaml"
        } else {
            "sql"
        };
        q.push_str("\n\nReviewed target current file content:\n");
        q.push_str("File: ");
        q.push_str(&rel);
        q.push_str("\n\n```");
        q.push_str(fence_lang);
        q.push_str("\n");
        q.push_str(&content);
        if !content.ends_with('\n') {
            q.push('\n');
        }
        q.push_str("```\n");
        rendered += 1;
    }
}

async fn transition_plan_missing(
    thread_store: &ThreadStore,
    thread_id: &str,
    phase: Phase,
    track: TrackKind,
) -> Result<(), String> {
    crate::phase_contract::commit_phase_decision(
        thread_store,
        thread_id,
        Some(phase),
        crate::phase_contract::PhaseDecision::loopback(
            track.plan_phase(),
            Some(crate::progress_controller::PhaseTransition::PlanMissing {
                kind: match track {
                    crate::track_spec::TrackKind::Cleanse => {
                        crate::progress_controller::TrackKind::Cleanse
                    }
                    crate::track_spec::TrackKind::Model => {
                        crate::progress_controller::TrackKind::Model
                    }
                },
                note: format!(
                    "authoring entered without an active {} plan; routing back to planning",
                    track.as_str()
                ),
            }),
        ),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_model_plan() -> crate::plan::ModelPlan {
        let batches = vec![vec!["dim_customers".to_string()]];
        crate::plan::ModelPlan {
            plan_key: "t/w/p/plans/tid/sample_model.json".to_string(),
            status: crate::plan::PlanStatus::Approved,
            project_snapshot: Default::default(),
            tasks: vec![crate::plan::ModelTask {
                name: "dim_customers".to_string(),
                folder: crate::plan::ModelFolder::Marts,
                goal: "build customer dimension".to_string(),
                inputs: vec!["stg_test_raw_raw_customers".to_string()],
                expected_model_path: Some("models/marts/dim_customers.sql".to_string()),
                invariants: vec![],
                implementation_spec: Some(crate::plan::ModelImplementationSpec {
                    spec_version: 1,
                    grain: "1 row per customer".to_string(),
                    inputs: vec!["stg_test_raw_raw_customers".to_string()],
                    joins: vec![],
                    metrics: vec![],
                    output_fields: vec![crate::plan::OutputFieldSpec {
                        name: "customer_id".to_string(),
                        kind: crate::plan::FieldKind::Clean,
                        source_columns: vec!["customer_id".to_string()],
                        expression: "customer_id passthrough".to_string(),
                        data_type: None,
                        nullable: true,
                        description: None,
                    }],
                    assumptions: vec![],
                    evidence_claim_refs: vec![crate::providers::SemanticClaimRef {
                        claim_id: "candidate_key:test_raw.raw_customers:customer_id"
                            .to_string()
                            .into(),
                        kind: crate::providers::SemanticClaimKind::CandidateKey,
                        status: crate::providers::EvidenceStatus::Observed,
                    }],
                }),
                source_schema: vec![crate::plan::SourceColumnDef {
                    name: "customer_id".to_string(),
                    data_type: "bigint".to_string(),
                }],
                grounded_inputs: vec![crate::plan::GroundedModelInput {
                    input_name: "stg_test_raw_raw_customers".to_string(),
                    model_rel_path: "models/staging/stg_test_raw_raw_customers.sql".to_string(),
                    relation_fqn: "catalog.db.stg_test_raw_raw_customers".to_string(),
                    source_schema: vec![crate::plan::SourceColumnDef {
                        name: "customer_id".to_string(),
                        data_type: "bigint".to_string(),
                    }],
                }],
                status: crate::plan::TaskStatus::InProgress,
                checklist: crate::plan::canonical_task_checklist(
                    crate::track_spec::TrackKind::Model,
                ),
            }],
            batches: batches.clone(),
            work_groups: crate::plan::canonical_work_groups_from_batches(&batches, "model"),
            mutations: vec![],
            progress: crate::plan::PlanProgress::default(),
        }
    }

    #[test]
    fn author_validate_trigger_is_single_source() {
        let incomplete = crate::plan::PlanCompletionSnapshot {
            all_done: false,
            pending_count: 1,
            pending_refs: vec![],
        };
        let complete = crate::plan::PlanCompletionSnapshot {
            all_done: true,
            pending_count: 0,
            pending_refs: vec![],
        };
        assert_eq!(
            decide_author_validate_trigger(
                &crate::plan::AuthoringNextAction::Validate,
                &incomplete,
                true
            ),
            None
        );
        assert_eq!(
            decide_author_validate_trigger(
                &crate::plan::AuthoringNextAction::Validate,
                &incomplete,
                false
            ),
            Some(AuthorValidateTrigger::WorkGroupValidate)
        );
        assert_eq!(
            decide_author_validate_trigger(
                &crate::plan::AuthoringNextAction::None,
                &complete,
                false
            ),
            Some(AuthorValidateTrigger::PlanTasksDone)
        );
        assert_eq!(
            decide_author_validate_trigger(
                &crate::plan::AuthoringNextAction::AuthorSql(vec!["x".to_string()]),
                &incomplete,
                false
            ),
            None
        );
    }

    #[test]
    fn review_patch_target_resolution_maps_task_ids_and_paths() {
        let tasks = vec![
            (
                "AwsDataCatalog.test_raw.raw_orders".to_string(),
                Some("models/staging/stg_test_raw_raw_orders.sql".to_string()),
            ),
            (
                "stg_test_raw_raw_order_items".to_string(),
                Some("models/staging/stg_test_raw_raw_order_items.sql".to_string()),
            ),
        ];
        let targets = resolve_review_patch_target_paths(
            &[
                "AwsDataCatalog.test_raw.raw_orders".to_string(),
                "stg_test_raw_raw_order_items".to_string(),
                "models/schema.yml".to_string(),
            ],
            &tasks,
            |task| task.0.as_str(),
            |task| task.1.as_deref(),
        );
        assert_eq!(
            targets,
            vec![
                "models/schema.yml".to_string(),
                "models/staging/stg_test_raw_raw_order_items.sql".to_string(),
                "models/staging/stg_test_raw_raw_orders.sql".to_string(),
            ]
        );
    }

    #[test]
    fn reconcile_existing_model_sql_checklist_marks_contract_valid_items_done() {
        let mut plan = sample_model_plan();
        let item_names = vec!["dim_customers".to_string()];
        let contract_valid_items = std::collections::HashSet::from([String::from("dim_customers")]);

        let changed = reconcile_existing_model_sql_checklist(
            &mut plan,
            &item_names,
            crate::plan::CHECKLIST_SQL_MODEL,
            &contract_valid_items,
        );

        assert!(changed);
        let status = plan.tasks[0]
            .checklist
            .iter()
            .find(|it| it.checklist_item_id == crate::plan::CHECKLIST_SQL_MODEL)
            .map(|it| it.status)
            .expect("sql checklist item should exist");
        assert_eq!(status, crate::plan::ChecklistItemStatus::Done);
    }

    #[test]
    fn reconciled_model_sql_routes_next_action_to_schema_contract() {
        let mut plan = sample_model_plan();
        let item_names = vec!["dim_customers".to_string()];
        let contract_valid_items = std::collections::HashSet::from([String::from("dim_customers")]);

        assert!(reconcile_existing_model_sql_checklist(
            &mut plan,
            &item_names,
            crate::plan::CHECKLIST_SQL_MODEL,
            &contract_valid_items,
        ));

        assert_eq!(
            crate::plan::model_next_authoring_action(&plan),
            crate::plan::AuthoringNextAction::AuthorSchema(vec!["dim_customers".to_string()])
        );
    }

    #[test]
    fn reconcile_existing_model_sql_checklist_ignores_contract_invalid_items() {
        let mut plan = sample_model_plan();
        let item_names = vec!["dim_customers".to_string()];
        let contract_valid_items = std::collections::HashSet::new();

        let changed = reconcile_existing_model_sql_checklist(
            &mut plan,
            &item_names,
            crate::plan::CHECKLIST_SQL_MODEL,
            &contract_valid_items,
        );

        assert!(!changed);
        let status = plan.tasks[0]
            .checklist
            .iter()
            .find(|it| it.checklist_item_id == crate::plan::CHECKLIST_SQL_MODEL)
            .map(|it| it.status)
            .expect("sql checklist item should exist");
        assert_eq!(status, crate::plan::ChecklistItemStatus::Pending);
    }

    #[test]
    fn storage_error_summary_hides_verbose_missing_object_details() {
        let err = "storage error: s3 get_object failed: ServiceError(NoSuchKey: The specified key does not exist; request_id=abc)";

        assert!(storage_error_is_missing_object(err));
        assert_eq!(compact_storage_error_summary(err), "object not found");
    }

    #[test]
    fn storage_error_summary_truncates_non_missing_errors() {
        let err = format!("storage error: {}", "x".repeat(400));
        let summary = compact_storage_error_summary(&err);

        assert!(summary.starts_with("storage error: "));
        assert!(summary.ends_with("..."));
        assert!(summary.len() < err.len());
    }

    #[test]
    fn schema_yml_model_names_detect_missing_model_stanza() {
        let names = collect_model_names_from_schema_yml(
            r#"
version: 2
models:
  - name: dim_customers
    columns: []
"#,
        )
        .expect("schema yml should parse");

        assert!(names.contains("dim_customers"));
        assert!(!names.contains("fact_orders"));
    }

    #[test]
    fn review_patch_context_forbids_plan_edits_and_names_escape_hatch() {
        let ctx = build_review_patch_plan_context(
            TrackKind::Model,
            "plans/model.json",
            &["models/marts/fct_orders.sql".to_string()],
        );

        assert!(ctx.contains("Do NOT revise the approved plan/spec"));
        assert!(ctx.contains("PLAN_CHANGE_REQUIRED"));
        assert!(ctx.contains("exact contradiction"));
    }
}

async fn transition_plan_not_approved(
    thread_store: &ThreadStore,
    thread_id: &str,
    phase: Phase,
    track: TrackKind,
    status: crate::plan_types::PlanStatus,
) -> Result<(), String> {
    crate::phase_contract::commit_phase_decision(
        thread_store,
        thread_id,
        Some(phase),
        crate::phase_contract::PhaseDecision::loopback(
            track.plan_phase(),
            Some(
                crate::progress_controller::PhaseTransition::PlanNotApproved {
                    status: match status {
                        crate::plan_types::PlanStatus::Draft => {
                            crate::progress_controller::PlanStatus::Draft
                        }
                        crate::plan_types::PlanStatus::Approved
                        | crate::plan_types::PlanStatus::Completed
                        | crate::plan_types::PlanStatus::Cancelled => {
                            crate::progress_controller::PlanStatus::Unknown
                        }
                    },
                },
            ),
        ),
    )
    .await
}

async fn transition_to_track_validate_with_plan_key(
    thread_store: &ThreadStore,
    thread_id: &str,
    phase: Phase,
    track: TrackKind,
    transition: crate::progress_controller::PhaseTransition,
    _plan_key: String,
) -> Result<(), String> {
    crate::phase_contract::commit_phase_decision(
        thread_store,
        thread_id,
        Some(phase),
        crate::phase_contract::PhaseDecision::forward(track.validate_phase(), Some(transition)),
    )
    .await
}

async fn track_completion_snapshot_all_done(actx: &AgentCtx, track: TrackKind) -> bool {
    if track.is_cleanse() {
        crate::plan::load_cleanse_plan(actx)
            .await
            .ok()
            .flatten()
            .map(|p| {
                crate::plan::snapshot_cleanse_completion(&p).completion_state()
                    == crate::plan::PlanCompletionState::Complete
            })
            .unwrap_or(false)
    } else {
        crate::plan::load_model_plan(actx)
            .await
            .ok()
            .flatten()
            .map(|p| {
                crate::plan::snapshot_model_completion(&p).completion_state()
                    == crate::plan::PlanCompletionState::Complete
            })
            .unwrap_or(false)
    }
}

fn resolve_checklist_item_id(actx: &AgentCtx, default: &str) -> String {
    actx.exec_ctx()
        .as_ref()
        .and_then(|c| c.get_str("checklist_item_id"))
        .unwrap_or(default)
        .trim()
        .to_string()
}

fn reconcile_existing_model_sql_checklist(
    plan: &mut crate::plan::ModelPlan,
    item_names: &[String],
    checklist_item_id: &str,
    contract_valid_item_names: &std::collections::HashSet<String>,
) -> bool {
    if checklist_item_id.trim().is_empty() {
        return false;
    }

    let mut changed = false;
    for item_name in item_names {
        let Some(task) = plan.tasks.iter().find(|t| t.name == *item_name) else {
            continue;
        };
        if !contract_valid_item_names.contains(item_name) {
            continue;
        }

        let already_done = task
            .checklist
            .iter()
            .find(|it| it.checklist_item_id == checklist_item_id)
            .map(|it| it.status == crate::plan::ChecklistItemStatus::Done)
            .unwrap_or(false);
        if already_done {
            continue;
        }

        changed = true;
        if checklist_item_id == crate::plan::CHECKLIST_SQL_MODEL {
            crate::plan::model_mark_done(plan, item_name);
        } else {
            crate::plan::model_checklist_mark_status(
                plan,
                item_name,
                checklist_item_id,
                crate::plan::ChecklistItemStatus::Done,
            );
        }
    }

    changed
}

async fn reconcile_existing_model_sql_from_storage(
    actx: &AgentCtx,
    plan: &mut crate::plan::ModelPlan,
    item_names: &[String],
    checklist_item_id: &str,
) -> bool {
    let mut contract_valid_item_names: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    for item_name in item_names {
        let Some(task) = plan.tasks.iter().find(|t| t.name == *item_name) else {
            continue;
        };
        let Some(expected_path) = task.expected_model_path.clone() else {
            continue;
        };
        let key = crate::project_fs::join_storage_key(actx, &expected_path);
        match retry_get_bytes(actx.storage().as_ref(), &key).await {
            Ok(bytes) => {
                let sql = String::from_utf8_lossy(&bytes).to_string();
                let check = crate::authoring_contract::verify_model_sql_contract(
                    &plan.plan_key,
                    task,
                    &sql,
                    true,
                );
                if check.is_ok() {
                    contract_valid_item_names.insert(item_name.clone());
                } else {
                    tracing::info!(
                        item_name = %item_name,
                        expected_model_path = %expected_path,
                        drift = ?check.drift_reasons,
                        "model SQL reconciliation refused stale/off-contract file"
                    );
                }
            }
            Err(e) => {
                let error_text = e.to_string();
                let error_summary = compact_storage_error_summary(&error_text);
                if storage_error_is_missing_object(&error_text) {
                    tracing::info!(
                        item_name = %item_name,
                        expected_model_path = %expected_path,
                        error_summary = %error_summary,
                        "model SQL reconciliation found no existing file; item will be authored"
                    );
                } else {
                    tracing::warn!(
                        item_name = %item_name,
                        expected_model_path = %expected_path,
                        error_summary = %error_summary,
                        "model SQL reconciliation could not read existing file; leaving item eligible for authoring"
                    );
                }
            }
        }
    }
    reconcile_existing_model_sql_checklist(
        plan,
        item_names,
        checklist_item_id,
        &contract_valid_item_names,
    )
}

fn storage_error_is_missing_object(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("nosuchkey")
        || lower.contains("not found")
        || lower.contains("statuscode(404)")
        || lower.contains("status: 404")
        || lower.contains("the specified key does not exist")
}

fn compact_storage_error_summary(error: &str) -> String {
    if storage_error_is_missing_object(error) {
        return "object not found".to_string();
    }

    let trimmed = error.trim();
    if trimmed.is_empty() {
        return "unknown storage error".to_string();
    }

    const MAX_SUMMARY_CHARS: usize = 240;
    if trimmed.chars().count() <= MAX_SUMMARY_CHARS {
        return trimmed.to_string();
    }

    let mut out: String = trimmed.chars().take(MAX_SUMMARY_CHARS).collect();
    out.push_str("...");
    out
}

fn collect_model_names_from_schema_yml(
    content: &str,
) -> Result<std::collections::HashSet<String>, serde_yaml::Error> {
    let vy = serde_yaml::from_str::<serde_yaml::Value>(content)?;
    let mut names_in_schema: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Some(models) = vy.get("models").and_then(|m| m.as_sequence()) {
        for m in models.iter() {
            if let Some(nm) = m
                .get("name")
                .and_then(|n| n.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
            {
                names_in_schema.insert(nm);
            }
        }
    }
    Ok(names_in_schema)
}

fn build_schema_checklist_context(
    track: TrackKind,
    plan_key: &str,
    checklist_item_id: &str,
    ids: &[String],
    expected_paths: &[String],
) -> String {
    let kind = track.as_str();
    let batch_tool = if track.is_cleanse() {
        "apply_next_cleanse_schema_batch"
    } else {
        "apply_next_model_schema_batch"
    };
    let mut ctx = format!(
        "Approved {kind} plan (stored at: {plan_key}).\nPending schema checklist work (checklist_item_id={checklist_item_id} ; max {}):\n- {}\n\nNext action: call {batch_tool} (do NOT call file directly).\n\nExpected model SQL paths:\n- {}\n",
        crate::plan_progress::MAX_BATCH_SIZE,
        ids.join("\n- "),
        expected_paths.join("\n- "),
    );
    ctx.push_str("\nIMPORTANT: Do NOT call the SQL batch-authoring tool while schema checklist work remains; continue schema checklist repairs first.\n");
    ctx
}

async fn check_batch_lock_and_loopback(
    actx: &AgentCtx,
    thread_store: &ThreadStore,
    thread_id: &str,
    phase: Phase,
    track: TrackKind,
    plan_key: &str,
    consecutive_batch_failures: usize,
    total_batch_failures: usize,
    next_ids: &[String],
    expected_paths: &[String],
) -> Result<Option<PhaseOutcome>, PhaseError> {
    if consecutive_batch_failures < crate::controller_kernel::max_consecutive_batch_failures() {
        return Ok(None);
    }
    let reason = crate::controller_kernel::build_batch_lock_prompt(
        track,
        plan_key,
        consecutive_batch_failures,
        total_batch_failures,
        next_ids,
        expected_paths,
    );
    let label = if track.is_cleanse() {
        "Cleanse"
    } else {
        "Model"
    };
    let detail = format!(
        "{label} batch authoring did not converge within the local retry budget. \
This is an implementation/authoring failure, not an implicit plan rewrite.\n\n{reason}"
    );
    cancel_active_plan_for_track(actx, track, "batch authoring retry budget exhausted").await?;
    Ok(Some(
        apply_author_escalation(
            thread_store,
            thread_id,
            phase,
            AuthorEscalation::FatalLocal(detail),
        )
        .await?,
    ))
}

async fn cancel_active_plan_for_track(
    actx: &AgentCtx,
    track: TrackKind,
    reason: &str,
) -> Result<(), String> {
    if track.is_cleanse() {
        if let Some(mut plan) = crate::plan::load_cleanse_plan(actx)
            .await
            .map_err(|e| e.to_string())?
        {
            if !plan.status.is_terminal() {
                tracing::warn!(plan_key = %plan.plan_key, reason = %reason, "cancelling active cleanse plan before terminal authoring failure");
                plan.status = crate::plan::PlanStatus::Cancelled;
                crate::plan::save_cleanse_plan(actx, &plan)
                    .await
                    .map_err(|e| e.to_string())?;
            }
        }
    } else if let Some(mut plan) = crate::plan::load_model_plan(actx)
        .await
        .map_err(|e| e.to_string())?
    {
        if !plan.status.is_terminal() {
            tracing::warn!(plan_key = %plan.plan_key, reason = %reason, "cancelling active model plan before terminal authoring failure");
            plan.status = crate::plan::PlanStatus::Cancelled;
            crate::plan::save_model_plan(actx, &plan)
                .await
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn author_plan_defect(phase: Phase, message: impl Into<String>) -> AuthorEscalation {
    AuthorEscalation::PlanDefect {
        violations: vec![crate::progress_controller::PlanViolation::new(
            phase,
            None,
            message.into(),
        )],
        strategy: crate::progress_controller::PlanRevisionStrategy::Rewrite,
    }
}

async fn apply_author_plan_defect(
    thread_store: &ThreadStore,
    thread_id: &str,
    phase: Phase,
    message: impl Into<String>,
) -> Result<PhaseOutcome, PhaseError> {
    apply_author_escalation(
        thread_store,
        thread_id,
        phase,
        author_plan_defect(phase, message),
    )
    .await
}

fn extract_validate_fail_context_for_cleanse(
    plan_key: &str,
    snapshot: &crate::plan_types::PlanSnapshot,
) -> Option<serde_json::Value> {
    let facts: Vec<_> = snapshot
        .validate_fail_facts
        .iter()
        .filter(|bundle| {
            bundle
                .plan_binding
                .as_ref()
                .and_then(|binding| binding.plan_key.as_deref())
                .map(|bound| bound == plan_key)
                .unwrap_or(true)
        })
        .cloned()
        .collect();
    if facts.is_empty() {
        None
    } else {
        serde_json::to_value(&facts).ok()
    }
}

fn extract_validate_fail_context_for_model(
    plan: &crate::plan::ModelPlan,
) -> Option<serde_json::Value> {
    let current_digests: std::collections::BTreeMap<String, String> = plan
        .tasks
        .iter()
        .filter_map(|task| {
            crate::authoring_contract::model_task_spec_digest(&plan.plan_key, task)
                .map(|digest| (task.name.clone(), digest))
        })
        .collect();
    let facts: Vec<_> = plan
        .project_snapshot
        .validate_fail_facts
        .iter()
        .filter(|bundle| {
            validate_fail_fact_matches_model_plan(&plan.plan_key, &current_digests, bundle)
        })
        .cloned()
        .collect();
    if facts.is_empty() {
        None
    } else {
        serde_json::to_value(&facts).ok()
    }
}

fn validate_fail_fact_matches_model_plan(
    plan_key: &str,
    current_digests: &std::collections::BTreeMap<String, String>,
    bundle: &crate::facts::FactsBundle,
) -> bool {
    let Some(binding) = bundle.plan_binding.as_ref() else {
        return false;
    };
    if binding.plan_key.as_deref() != Some(plan_key) {
        return false;
    }
    binding.task_spec_digests.iter().all(|(task, digest)| {
        current_digests
            .get(task)
            .map(|current| current == digest)
            .unwrap_or(false)
    })
}

async fn mark_off_contract_model_artifacts_from_storage(
    actx: &AgentCtx,
    plan: &mut crate::plan::ModelPlan,
) -> Result<bool, PhaseError> {
    let mut sql_updates: Vec<(String, String)> = Vec::new();
    for task in plan.tasks.iter() {
        let Some(expected_path) = task.expected_model_path.as_deref() else {
            continue;
        };
        let key = crate::project_fs::join_storage_key(actx, expected_path);
        match retry_get_bytes(actx.storage().as_ref(), &key).await {
            Ok(bytes) => {
                let sql = String::from_utf8_lossy(&bytes).to_string();
                let check = crate::authoring_contract::verify_model_sql_contract(
                    &plan.plan_key,
                    task,
                    &sql,
                    true,
                );
                if !check.is_ok() && model_checklist_is_done(task, crate::plan::CHECKLIST_SQL_MODEL)
                {
                    let status = crate::authoring_contract::ArtifactContractStatus::OffContract(
                        check.drifts.clone(),
                    );
                    if status.repair_route()
                        == crate::authoring_contract::RepairRoute::ReconcileToPlan
                    {
                        sql_updates.push((
                            task.name.clone(),
                            format!(
                                "Plan-owned SQL is stale/off-contract and must be reconciled before repair: {}",
                                check.drift_reasons.join("; ")
                            ),
                        ));
                    }
                }
            }
            Err(e) => {
                let error_text = e.to_string();
                if storage_error_is_missing_object(&error_text)
                    && model_checklist_is_done(task, crate::plan::CHECKLIST_SQL_MODEL)
                {
                    let status = crate::authoring_contract::ArtifactContractStatus::Missing;
                    if status.repair_route()
                        == crate::authoring_contract::RepairRoute::ReconcileToPlan
                    {
                        sql_updates.push((
                            task.name.clone(),
                            "Plan-owned SQL is missing and must be authored before repair."
                                .to_string(),
                        ));
                    }
                }
            }
        }
    }

    let mut schema_updates: Vec<(String, String)> = Vec::new();
    let schema_key =
        crate::project_fs::join_storage_key(actx, crate::project_fs::MODELS_SCHEMA_YML);
    match retry_get_bytes(actx.storage().as_ref(), &schema_key).await {
        Ok(bytes) => {
            let schema = String::from_utf8_lossy(&bytes).to_string();
            for task in plan.tasks.iter() {
                let Some(spec) = task.implementation_spec.as_ref() else {
                    continue;
                };
                let check = crate::authoring_contract::verify_model_schema_yml_contract(
                    &task.name,
                    &spec.output_fields,
                    &schema,
                );
                if !check.is_ok()
                    && model_checklist_is_done(task, crate::plan::CHECKLIST_SCHEMA_CONTRACT)
                {
                    schema_updates.push((
                        task.name.clone(),
                        format!(
                            "Plan-owned schema.yml stanza is stale/off-contract and must be reconciled before repair: {}",
                            check.drift_reasons.join("; ")
                        ),
                    ));
                }
            }
        }
        Err(e) => {
            if storage_error_is_missing_object(&e.to_string()) {
                for task in plan.tasks.iter() {
                    if !model_checklist_is_done(task, crate::plan::CHECKLIST_SCHEMA_CONTRACT) {
                        continue;
                    }
                    schema_updates.push((
                        task.name.clone(),
                        "models/schema.yml is missing and must be authored before repair."
                            .to_string(),
                    ));
                }
            }
        }
    }

    let changed = !sql_updates.is_empty() || !schema_updates.is_empty();
    for (name, reason) in sql_updates {
        crate::plan::model_mark_needs_update(plan, &name, Some(&reason));
    }
    for (name, reason) in schema_updates {
        crate::plan::model_schema_contract_mark_needs_update(plan, &name, Some(&reason));
    }
    Ok(changed)
}

fn model_checklist_is_done(task: &crate::plan::ModelTask, checklist_item_id: &str) -> bool {
    task.checklist
        .iter()
        .find(|item| item.checklist_item_id == checklist_item_id)
        .map(|item| item.status == crate::plan::ChecklistItemStatus::Done)
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Extracted from execute_author_phase: plan loading (cleanse track)
// ---------------------------------------------------------------------------

async fn load_cleanse_author_context(
    params: &AuthorPhaseCtx<'_>,
    actx: &mut AgentCtx,
) -> Result<AuthorPlanLoadResult, PhaseError> {
    let plan = match crate::plan::load_cleanse_plan(actx).await? {
        Some(p) => p,
        None => {
            transition_plan_missing(
                params.thread_store,
                params.thread_id,
                params.phase,
                params.track,
            )
            .await?;
            return Ok(AuthorPlanLoadResult::EarlyReturn(
                PhaseOutcome::TransitionCommitted,
            ));
        }
    };
    if let control_flow::AuthoringGate::Block { reason } =
        control_flow::gate_author_phase_execution(&plan)
    {
        return Ok(AuthorPlanLoadResult::EarlyReturn(
            apply_author_plan_defect(
                params.thread_store,
                params.thread_id,
                params.phase,
                format!("Plan is not executable: {reason}"),
            )
            .await?,
        ));
    }
    let _ = crate::plan::save_cleanse_plan(actx, &plan).await;

    let next_item = crate::plan::cleanse_next_work_item_ctx(&plan);
    crate::phase_author_lifecycle::bind_execution_context(
        actx,
        params.track,
        plan.plan_key.clone(),
        next_item,
    );
    {
        let next = crate::plan::cleanse_next_authoring_action(&plan).author_sql_ids();
        let expected_paths = crate::phase_author_lifecycle::collect_expected_paths(
            &plan.tasks,
            &next,
            |task| task.dataset_id.as_str(),
            |task| task.expected_model_path.as_deref(),
        );
        if let Some(outcome) = check_batch_lock_and_loopback(
            actx,
            params.thread_store,
            params.thread_id,
            params.phase,
            params.track,
            &plan.plan_key,
            plan.progress.consecutive_batch_failures,
            plan.progress.total_batch_failures,
            &next,
            &expected_paths,
        )
        .await?
        {
            return Ok(AuthorPlanLoadResult::EarlyReturn(outcome));
        }
    }
    if plan.status != crate::plan::PlanStatus::Approved
        && plan.status != crate::plan::PlanStatus::Completed
    {
        transition_plan_not_approved(
            params.thread_store,
            params.thread_id,
            params.phase,
            params.track,
            plan.status,
        )
        .await?;
        return Ok(AuthorPlanLoadResult::EarlyReturn(
            PhaseOutcome::TransitionCommitted,
        ));
    }

    let next_action = crate::plan::cleanse_next_authoring_action(&plan);
    let next = next_action.author_sql_ids();

    if crate::phase_gate::patch_impl_intent_unsatisfied(params.execution_state, params.phase) {
        let review_target_paths = review_patch_detail(params.execution_state)
            .map(|detail| {
                resolve_review_patch_target_paths(
                    &detail.meta.target_task_ids,
                    &plan.tasks,
                    |task| task.dataset_id.as_str(),
                    |task| task.expected_model_path.as_deref(),
                )
            })
            .unwrap_or_default();
        return Ok(AuthorPlanLoadResult::Ready {
            plan_context: build_review_patch_plan_context(
                params.track,
                &plan.plan_key,
                &review_target_paths,
            ),
            plan_state: PlanState::Repair,
        });
    }

    if next.is_empty() {
        if let crate::plan::AuthoringNextAction::AuthorSchema(ids) = &next_action {
            let checklist_item_id =
                resolve_checklist_item_id(actx, crate::plan::CHECKLIST_SCHEMA_CONTRACT);
            let expected_paths = crate::phase_author_lifecycle::collect_expected_paths(
                &plan.tasks,
                ids,
                |task| task.dataset_id.as_str(),
                |task| task.expected_model_path.as_deref(),
            );
            let ctx = build_schema_checklist_context(
                params.track,
                &plan.plan_key,
                &checklist_item_id,
                ids,
                &expected_paths,
            );
            Ok(AuthorPlanLoadResult::Ready {
                plan_context: ctx,
                plan_state: PlanState::CleanseSchemaDatasetIds(ids.clone()),
            })
        } else {
            let completion_snapshot = crate::plan::snapshot_cleanse_completion(&plan);
            if let Some(trigger) = decide_author_validate_trigger(
                &next_action,
                &completion_snapshot,
                params.last_validate_failed,
            ) {
                let transition = match trigger {
                    AuthorValidateTrigger::WorkGroupValidate => {
                        crate::progress_controller::PhaseTransition::WorkGroupValidate
                    }
                    AuthorValidateTrigger::PlanTasksDone => {
                        crate::progress_controller::PhaseTransition::PlanTasksDone
                    }
                };
                transition_to_track_validate_with_plan_key(
                    params.thread_store,
                    params.thread_id,
                    params.phase,
                    params.track,
                    transition,
                    plan.plan_key.clone(),
                )
                .await?;
                return Ok(AuthorPlanLoadResult::EarlyReturn(
                    PhaseOutcome::TransitionCommitted,
                ));
            }

            if params.last_validate_failed {
                Ok(AuthorPlanLoadResult::Ready {
                    plan_context: build_post_validate_fail_repair_context(
                        params.track,
                        &plan.plan_key,
                        params.repair_ctx,
                    ),
                    plan_state: PlanState::Repair,
                })
            } else {
                Ok(AuthorPlanLoadResult::EarlyReturn(
                    apply_author_plan_defect(
                        params.thread_store,
                        params.thread_id,
                        params.phase,
                        "Approved cleanse plan is not executable: no next work-group action while checklist work remains",
                    )
                    .await?,
                ))
            }
        }
    } else {
        Ok(AuthorPlanLoadResult::Ready {
            plan_context: format!(
                "Approved cleanse plan (stored at: {}).\nNext batch (deterministic, max {}):\n- {}\n\nNext action: call the deterministic batch authoring tool from the current tool card (do NOT call staging_model directly).",
                crate::plan_progress::MAX_BATCH_SIZE,
                plan.plan_key,
                next.join("\n- ")
            ),
            plan_state: PlanState::CleanseSqlDatasetIds(next.clone()),
        })
    }
}

// ---------------------------------------------------------------------------
// Extracted from execute_author_phase: plan loading (model track)
// ---------------------------------------------------------------------------

async fn load_model_author_context(
    params: &AuthorPhaseCtx<'_>,
    actx: &mut AgentCtx,
) -> Result<AuthorPlanLoadResult, PhaseError> {
    let mut plan = match crate::plan::load_model_plan(actx).await? {
        Some(p) => p,
        None => {
            transition_plan_missing(
                params.thread_store,
                params.thread_id,
                params.phase,
                params.track,
            )
            .await?;
            return Ok(AuthorPlanLoadResult::EarlyReturn(
                PhaseOutcome::TransitionCommitted,
            ));
        }
    };
    if let control_flow::AuthoringGate::Block { reason } =
        control_flow::gate_author_phase_execution(&plan)
    {
        return Ok(AuthorPlanLoadResult::EarlyReturn(
            apply_author_plan_defect(
                params.thread_store,
                params.thread_id,
                params.phase,
                format!("Plan is not executable: {reason}"),
            )
            .await?,
        ));
    }
    let _ = crate::plan::save_model_plan(actx, &plan).await;

    let next_item = crate::plan::model_next_work_item_ctx(&plan);
    crate::phase_author_lifecycle::bind_execution_context(
        actx,
        params.track,
        plan.plan_key.clone(),
        next_item,
    );
    if params.last_validate_failed
        && mark_off_contract_model_artifacts_from_storage(actx, &mut plan).await?
    {
        crate::plan::save_model_plan(actx, &plan).await?;
        return Ok(AuthorPlanLoadResult::EarlyReturn(
            PhaseOutcome::stayed_with_progress(
                "marked stale plan-owned model artifacts for reconciliation before repair",
            ),
        ));
    }
    let next_action = crate::plan::model_next_authoring_action(&plan);
    let next_names = next_action.author_sql_ids();
    if !next_names.is_empty() {
        let checklist_item_id = resolve_checklist_item_id(actx, crate::plan::CHECKLIST_SQL_MODEL);
        if reconcile_existing_model_sql_from_storage(
            actx,
            &mut plan,
            &next_names,
            &checklist_item_id,
        )
        .await
        {
            crate::plan::save_model_plan(actx, &plan).await?;
            return Ok(AuthorPlanLoadResult::EarlyReturn(
                PhaseOutcome::stayed_with_progress(
                    "marked existing model SQL checklist items done after reconciling written gold SQL",
                ),
            ));
        }
    }
    {
        let expected_paths = crate::phase_author_lifecycle::collect_expected_paths(
            &plan.tasks,
            &next_names,
            |task| task.name.as_str(),
            |task| task.expected_model_path.as_deref(),
        );
        if let Some(outcome) = check_batch_lock_and_loopback(
            actx,
            params.thread_store,
            params.thread_id,
            params.phase,
            params.track,
            &plan.plan_key,
            plan.progress.consecutive_batch_failures,
            plan.progress.total_batch_failures,
            &next_names,
            &expected_paths,
        )
        .await?
        {
            return Ok(AuthorPlanLoadResult::EarlyReturn(outcome));
        }
    }
    if plan.status != crate::plan::PlanStatus::Approved
        && plan.status != crate::plan::PlanStatus::Completed
    {
        transition_plan_not_approved(
            params.thread_store,
            params.thread_id,
            params.phase,
            params.track,
            plan.status,
        )
        .await?;
        return Ok(AuthorPlanLoadResult::EarlyReturn(
            PhaseOutcome::TransitionCommitted,
        ));
    }

    if crate::phase_gate::patch_impl_intent_unsatisfied(params.execution_state, params.phase) {
        let review_target_paths = review_patch_detail(params.execution_state)
            .map(|detail| {
                resolve_review_patch_target_paths(
                    &detail.meta.target_task_ids,
                    &plan.tasks,
                    |task| task.name.as_str(),
                    |task| task.expected_model_path.as_deref(),
                )
            })
            .unwrap_or_default();
        return Ok(AuthorPlanLoadResult::Ready {
            plan_context: build_review_patch_plan_context(
                params.track,
                &plan.plan_key,
                &review_target_paths,
            ),
            plan_state: PlanState::Repair,
        });
    }

    if next_names.is_empty() {
        if let crate::plan::AuthoringNextAction::AuthorSchema(ids) = &next_action {
            // Reconcile: if models/schema.yml already contains stanzas for all
            // pending models, mark the checklist items done and report progress.
            // Otherwise fall through to present the schema batch tool to the LLM.
            {
                let key =
                    crate::project_fs::join_storage_key(actx, crate::project_fs::MODELS_SCHEMA_YML);
                if let Ok(bytes) = retry_get_bytes(actx.storage().as_ref(), &key).await {
                    let content = String::from_utf8_lossy(&bytes).to_string();
                    if collect_model_names_from_schema_yml(&content).is_ok() {
                        let mut all_contract_valid = true;
                        for n in ids.iter() {
                            let Some(task) = plan.tasks.iter().find(|t| t.name == *n) else {
                                all_contract_valid = false;
                                break;
                            };
                            let Some(spec) = task.implementation_spec.as_ref() else {
                                all_contract_valid = false;
                                break;
                            };
                            let check = crate::authoring_contract::verify_model_schema_yml_contract(
                                n,
                                &spec.output_fields,
                                &content,
                            );
                            if !check.is_ok() {
                                tracing::info!(
                                    item_name = %n,
                                    drift = ?check.drift_reasons,
                                    "model schema reconciliation refused stale/off-contract schema.yml entry"
                                );
                                all_contract_valid = false;
                                break;
                            }
                        }
                        if all_contract_valid {
                            let mut changed = false;
                            let checklist_item_id = resolve_checklist_item_id(
                                actx,
                                crate::plan::CHECKLIST_SCHEMA_CONTRACT,
                            );
                            for n in ids.iter() {
                                if let Some(t) = plan.tasks.iter().find(|t| t.name == *n) {
                                    let done = t
                                        .checklist
                                        .iter()
                                        .find(|it| it.checklist_item_id == checklist_item_id)
                                        .map(|it| {
                                            it.status == crate::plan::ChecklistItemStatus::Done
                                        })
                                        .unwrap_or(false);
                                    if !done {
                                        changed = true;
                                    }
                                }
                                crate::plan::model_checklist_mark_status(
                                    &mut plan,
                                    n,
                                    &checklist_item_id,
                                    crate::plan::ChecklistItemStatus::Done,
                                );
                            }
                            if changed {
                                crate::plan::save_model_plan(actx, &plan).await?;
                                return Ok(AuthorPlanLoadResult::EarlyReturn(
                                    PhaseOutcome::stayed_with_progress(
                                        "marked existing schema checklist items done after reconciling models/schema.yml",
                                    ),
                                ));
                            }
                        }
                    }
                }
            }

            let expected_paths = crate::phase_author_lifecycle::collect_expected_paths(
                &plan.tasks,
                ids,
                |task| task.name.as_str(),
                |task| task.expected_model_path.as_deref(),
            );
            let checklist_item_id =
                resolve_checklist_item_id(actx, crate::plan::CHECKLIST_SCHEMA_CONTRACT);
            let ctx = build_schema_checklist_context(
                params.track,
                &plan.plan_key,
                &checklist_item_id,
                ids,
                &expected_paths,
            );
            Ok(AuthorPlanLoadResult::Ready {
                plan_context: ctx,
                plan_state: PlanState::Unconstrained,
            })
        } else {
            let completion_snapshot = crate::plan::snapshot_model_completion(&plan);
            if let Some(trigger) = decide_author_validate_trigger(
                &next_action,
                &completion_snapshot,
                params.last_validate_failed,
            ) {
                let transition = match trigger {
                    AuthorValidateTrigger::WorkGroupValidate => {
                        crate::progress_controller::PhaseTransition::WorkGroupValidate
                    }
                    AuthorValidateTrigger::PlanTasksDone => {
                        crate::progress_controller::PhaseTransition::PlanTasksDone
                    }
                };
                transition_to_track_validate_with_plan_key(
                    params.thread_store,
                    params.thread_id,
                    params.phase,
                    params.track,
                    transition,
                    plan.plan_key.clone(),
                )
                .await?;
                return Ok(AuthorPlanLoadResult::EarlyReturn(
                    PhaseOutcome::TransitionCommitted,
                ));
            }

            if params.last_validate_failed {
                Ok(AuthorPlanLoadResult::Ready {
                    plan_context: build_post_validate_fail_repair_context(
                        params.track,
                        &plan.plan_key,
                        params.repair_ctx,
                    ),
                    plan_state: PlanState::Repair,
                })
            } else {
                Ok(AuthorPlanLoadResult::EarlyReturn(
                    apply_author_plan_defect(
                        params.thread_store,
                        params.thread_id,
                        params.phase,
                        "Approved model plan is not executable: no next work-group action while checklist work remains",
                    )
                    .await?,
                ))
            }
        }
    } else {
        let allowed = PlanState::ModelSqlItemNames(next_names.clone());
        let mut details: Vec<String> = Vec::new();
        for n in next_names.iter() {
            if let Some(t) = plan.tasks.iter().find(|t| t.name == *n) {
                details.push(format!(
                    "- name: {}\n  folder: {}\n  goal: {}\n  inputs: {:?}",
                    t.name, t.folder, t.goal, t.inputs
                ));
            } else {
                details.push(format!("- name: {}", n));
            }
        }
        Ok(AuthorPlanLoadResult::Ready {
            plan_context: format!(
                "Approved model plan (stored at: {}).\nNext batch (deterministic, max {}):\n{}\n\nNext action: call the deterministic batch authoring tool from the current tool card (do NOT call gold_model directly).",
                crate::plan_progress::MAX_BATCH_SIZE,
                plan.plan_key,
                details.join("\n")
            ),
            plan_state: allowed,
        })
    }
}

// ---------------------------------------------------------------------------
// Extracted from execute_author_phase: prompt + LLM options construction
// ---------------------------------------------------------------------------

async fn build_author_prompt(
    params: &AuthorPhaseCtx<'_>,
    actx: &AgentCtx,
    question: &str,
    plan_context: &str,
    plan_state: &PlanState,
) -> Result<AuthorPrompt, PhaseError> {
    let mut q = if params.track.is_cleanse() {
        DataEngineerSuite::inject_cleanse_question(question)
    } else {
        DataEngineerSuite::inject_model_question(question)
    };
    if !params.track.is_cleanse() {
        let base = actx
            .keyspace()
            .scoped_prefix(actx.scope(), &["dbt"])
            .trim_end_matches('/')
            .to_string();
        let pref = format!("{}/models/staging/", base);
        if let Ok(keys) = retry_list_prefix(actx.storage().as_ref(), &pref).await {
            let mut rels: Vec<String> = keys
                .into_iter()
                .filter(|k| k.ends_with(".sql") && !k.contains("/_versions/"))
                .filter_map(|k| k.strip_prefix(&(base.clone() + "/")).map(|s| s.to_string()))
                .collect();
            rels.sort();
            rels.dedup();
            let mut names: Vec<String> = rels
                .into_iter()
                .filter_map(|rel| {
                    std::path::Path::new(&rel)
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                })
                .collect();
            names.sort();
            names.dedup();
            if !names.is_empty() {
                q.push_str("\n\nCurrent staged silver models (use ref() from these, plus any intra-plan gold models):\n");
                for n in names.into_iter().take(60) {
                    q.push_str("- ");
                    q.push_str(&n);
                    q.push('\n');
                }
            }
        }
    }
    q.push_str("\n\nNOTE: In agent mode, validation and publish are handled by the suite phases. Do not call dbt_validate or publish tools; focus on authoring fixes and models.");
    q.push_str("\nIMPORTANT: Tool-call argument shapes are strict. In particular: vect_query uses args.query_text (NOT args.query) and scope must be \"dataset\"|\"field\"|\"doc\"|\"artifact\"|\"metric\"|\"model\".");
    q.push_str("\nIMPORTANT: sql_stats requires args.field. Do not sample rows for semantic evidence; use aggregate run_sql queries only when needed.");
    q.push_str("\nIMPORTANT: This authoring phase is plan-driven. Follow the Plan context below. If it says to patch failing DBT files, do that first; if it provides a next batch, execute it. Do NOT ask for approval; approvals happen in plan phases.");
    q.push_str("\nIMPORTANT: No downstream compensation exists for incomplete plan structure. If execution context is incomplete, return to planning; do not invent fallback execution.");
    q.push_str("\n\nPlan context:\n");
    q.push_str(plan_context);
    {
        let dialect = crate::facts::SqlDialect(
            crate::resolved_config_from_ctx(actx)
                .as_ref()
                .map(|cfg| crate::dialect::active_provider_dialect(cfg))
                .unwrap_or_else(|| "Unknown SQL dialect".to_string()),
        );
        let mut batch_relations: Vec<String> = Vec::new();
        let mut prior_validate_facts: Option<serde_json::Value> = None;
        if params.track.is_cleanse() {
            if let Some(p) = crate::plan::load_cleanse_plan(actx).await? {
                if let PlanState::CleanseSqlDatasetIds(ds)
                | PlanState::CleanseSchemaDatasetIds(ds) = plan_state
                {
                    batch_relations = crate::facts::dataset_ids_to_fqns(ds);
                }
                prior_validate_facts =
                    extract_validate_fail_context_for_cleanse(&p.plan_key, &p.project_snapshot);
            }
        } else {
            if let Some(p) = crate::plan::load_model_plan(actx).await? {
                if let PlanState::ModelSqlItemNames(names) = plan_state {
                    {
                        let mut want_names: Vec<String> = names.clone();
                        for n in names.iter() {
                            if let Some(t) = p.tasks.iter().find(|t| t.name == *n) {
                                for inp in t.inputs.iter() {
                                    let s = inp.trim();
                                    if !s.is_empty() {
                                        want_names.push(s.to_string());
                                    }
                                }
                            }
                        }
                        want_names.sort();
                        want_names.dedup();
                        batch_relations =
                            crate::facts::resolve_model_names_to_fqns(actx, &want_names).await;
                    }
                }
                prior_validate_facts = extract_validate_fail_context_for_model(&p);
            }
        }

        if !batch_relations.is_empty() {
            let limits =
                crate::facts::FactsLimits::for_scope(crate::facts::FactsScope::AuthorBatch);
            let bundle = crate::facts::build_facts_bundle_from_relations(
                actx,
                crate::facts::FactsScope::AuthorBatch,
                dialect.clone(),
                &batch_relations,
                limits,
            )
            .await;
            q.push_str("\n\nIMMUTABLE FACTS (author_batch_schema):\n");
            q.push_str(&serde_json::to_string_pretty(&bundle).unwrap_or_else(|_| "{}".to_string()));
            q.push('\n');
            q.push_str("Rules:\n- You MUST NOT reference any column not present in facts.relations[].columns for that relation.\n- If required facts are missing, call sql_schema and then patch.\n");
        }
        if let Some(vf) = prior_validate_facts {
            q.push_str("\n\nIMMUTABLE FACTS (latest_validate_fail_facts):\n");
            q.push_str(&serde_json::to_string_pretty(&vf).unwrap_or_else(|_| "{}".to_string()));
            q.push('\n');
        }
    }
    if params.repair_ctx.has_context() {
        q.push_str("\n\n");
        q.push_str(&params.repair_ctx.format_error_context());
    }
    if matches!(
        params.execution_state.phase.transition.as_ref(),
        Some(crate::progress_controller::PhaseTransition::ReviewPatchImpl { .. })
    ) {
        let review_detail = review_patch_detail(params.execution_state);
        if let Some(key) = review_detail
            .as_ref()
            .and_then(|detail| detail.meta.review_ref.as_ref())
            .map(|review_ref| review_ref.key.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            if let Ok(bytes) = retry_get_bytes(actx.storage().as_ref(), &key).await {
                let txt = String::from_utf8_lossy(&bytes).to_string();
                if !txt.trim().is_empty() {
                    q.push_str("\n\nPRIOR REVIEW FEEDBACK (must address by editing implementation; do NOT change the approved plan/spec):\n");
                    q.push_str(txt.trim());
                    q.push('\n');
                }
            }
        }
        let review_target_paths = if params.track.is_cleanse() {
            crate::plan::load_cleanse_plan(actx)
                .await?
                .map(|plan| {
                    review_detail
                        .as_ref()
                        .map(|detail| {
                            resolve_review_patch_target_paths(
                                &detail.meta.target_task_ids,
                                &plan.tasks,
                                |task| task.dataset_id.as_str(),
                                |task| task.expected_model_path.as_deref(),
                            )
                        })
                        .unwrap_or_default()
                })
                .unwrap_or_default()
        } else {
            crate::plan::load_model_plan(actx)
                .await?
                .map(|plan| {
                    review_detail
                        .as_ref()
                        .map(|detail| {
                            resolve_review_patch_target_paths(
                                &detail.meta.target_task_ids,
                                &plan.tasks,
                                |task| task.name.as_str(),
                                |task| task.expected_model_path.as_deref(),
                            )
                        })
                        .unwrap_or_default()
                })
                .unwrap_or_default()
        };
        append_review_patch_target_contents(&mut q, actx, &review_target_paths).await;
    }
    if params.probe_required && !params.probe_satisfied {
        q.push_str("\n\nConstraint: runtime validation failed after compile; run meaningful run_sql probes (not SELECT 1) to diagnose data before re-validating. Multiple probes are allowed while they add new signal; repeated same/no-signal probes require you to switch to a mutating file fix.");
    }

    let has_models = control_flow::invariant_has_any_models(actx)
        .await
        .unwrap_or(false);
    if !has_models {
        q.push_str("\n\nIMPORTANT: invariant failed: there are no DBT model SQL files yet. Your first task is to create at least one staging model under models/ using staging_model or file op=patch.");
    }

    let llm_options = if params.track.is_cleanse() {
        let author_max_tokens: u32 =
            crate::env_util::env_u32(crate::env_util::env_keys::LLM_AUTHOR_MAX_TOKENS_CLEANSE)
                .unwrap_or(24_000)
                .max(2_000)
                .min(64_000);
        LlmCallOptions {
            prompt_id: "data_engineer.cleanse_author",
            thread_id: None,
            model: None,
            expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
            max_output_tokens: Some(author_max_tokens),
            reasoning_effort: Some(crate::env_util::author_reasoning_effort(true)),
            ..Default::default()
        }
    } else {
        let author_max_tokens: u32 =
            crate::env_util::env_u32(crate::env_util::env_keys::LLM_AUTHOR_MAX_TOKENS_MODEL)
                .unwrap_or(32_000)
                .max(2_000)
                .min(64_000);
        LlmCallOptions {
            prompt_id: "data_engineer.model_author",
            thread_id: None,
            model: None,
            expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
            max_output_tokens: Some(author_max_tokens),
            reasoning_effort: Some(crate::env_util::author_reasoning_effort(false)),
            ..Default::default()
        }
    };

    Ok(AuthorPrompt {
        question: q,
        llm_options,
    })
}

// ---------------------------------------------------------------------------
// Extracted from execute_author_phase: agent run outcome handling
// ---------------------------------------------------------------------------

async fn handle_author_run_outcome(
    params: &AuthorPhaseCtx<'_>,
    actx: &AgentCtx,
    sctx: &SuiteCtx,
    _pre_mutation_epoch: u64,
    outcome: RunOutcomeNonInteractive,
) -> Result<PhaseOutcome, PhaseError> {
    match outcome {
        RunOutcomeNonInteractive::Complete { .. } => {
            let has_proj = control_flow::invariant_has_dbt_project(actx)
                .await
                .unwrap_or(false);
            let has_models = control_flow::invariant_has_any_models(actx)
                .await
                .unwrap_or(false);
            if !has_proj || !has_models {
                return Ok(PhaseOutcome::stayed_waiting(
                    "authoring completed without a usable dbt project/model inventory yet",
                ));
            }
            let gate_state = crate::progress_controller::ExecutionState::load_strict(
                &params.thread_store.control_store(),
                params.thread_id,
            )
            .await?
            .unwrap_or_else(crate::progress_controller::ExecutionState::new);
            let patch_impl_mutation_satisfied = gate_state
                .repair
                .pending_patch_impl
                .as_ref()
                .map(|i| i.mutated_since_set)
                .unwrap_or(true);
            if let Err(reason) = crate::progress_controller::gate_authoring_probe(&gate_state) {
                apply_guard_block(
                    params.thread_store,
                    params.thread_id,
                    params.phase,
                    GuardBlockKind::AuthoringToValidate,
                    reason.clone(),
                )
                .await?;
                return Ok(PhaseOutcome::stayed_waiting(reason));
            }
            if crate::phase_gate::patch_impl_intent_unsatisfied(&gate_state, params.phase) {
                let reason = format!(
                    "progress_gate_blocked: review requested implementation patch for phase '{}' and no successful mutation has been recorded since loopback. Apply a mutating file op (patch/rm/mv) before re-validating.",
                    params.phase.as_str()
                );
                apply_guard_block(
                    params.thread_store,
                    params.thread_id,
                    params.phase,
                    GuardBlockKind::AuthoringToValidate,
                    reason.clone(),
                )
                .await?;
                return Ok(PhaseOutcome::stayed_waiting(reason));
            }
            if matches!(
                gate_state.repair.pending_patch_impl.as_ref(),
                Some(intent) if intent.phase == params.phase
            ) && patch_impl_mutation_satisfied
            {
                crate::state_manager::apply_execution_event(
                    &params.thread_store.control_store(),
                    params.thread_id,
                    crate::progress_controller::DataEngineerEvent::PatchImplIntentCleared,
                )
                .await
                .map_err(|e| format!("failed to clear pending patch-impl intent: {e}"))?;
            }

            if !params.track.is_cleanse() {
                let actx = DataEngineerSuite::agent_tool_ctx(params.thread_id, sctx);
                if !DataEngineerSuite::has_any_gold_model_sql(&actx).await {
                    let reason = "No gold models were found under models/core/ or models/marts/ after ModelAuthor. Gold must be explicitly authored (marts/core SQL) before validating/publishing.";
                    apply_guard_block(
                        params.thread_store,
                        params.thread_id,
                        params.phase,
                        GuardBlockKind::MissingGoldModels,
                        reason.to_string(),
                    )
                    .await?;
                    return Ok(PhaseOutcome::stayed_waiting(reason));
                }
            }

            if !track_completion_snapshot_all_done(actx, params.track).await {
                return Ok(PhaseOutcome::stayed_with_progress(
                    "authoring turn completed with durable progress but more plan work remains",
                ));
            }

            let to_phase = if params.track.is_cleanse() {
                Phase::CleanseValidate
            } else {
                Phase::ModelValidate
            };
            let (silver_count, gold_count) = if params.track.is_cleanse() {
                let n = crate::plan::load_cleanse_plan(actx)
                    .await
                    .ok()
                    .flatten()
                    .map(|p| p.tasks.len() as u64)
                    .unwrap_or(0);
                (n, 0u64)
            } else {
                let n = crate::plan::load_model_plan(actx)
                    .await
                    .ok()
                    .flatten()
                    .map(|p| p.tasks.len() as u64)
                    .unwrap_or(0);
                (0u64, n)
            };
            let _reason_detail = DataEngineerSuite::authoring_complete_reason_detail(
                params.thread_store,
                params.thread_id,
                has_proj,
                has_models,
            )
            .await;
            crate::phase_contract::commit_metered_decision(
                params.thread_store,
                params.thread_id,
                Some(params.phase),
                crate::phase_contract::PhaseDecision::forward(
                    to_phase,
                    Some(crate::progress_controller::PhaseTransition::AuthoringComplete),
                ),
                vec![crate::metering::UsageEvent::ModelsAuthored {
                    silver: silver_count,
                    gold: gold_count,
                    project_id: params.thread_id.to_string(),
                }],
                crate::metering::global_metering(),
            )
            .await?;
            Ok(PhaseOutcome::TransitionCommitted)
        }
        RunOutcomeNonInteractive::StepBoundary { .. } => {
            let post_state = crate::progress_controller::ExecutionState::load_strict(
                &params.thread_store.control_store(),
                params.thread_id,
            )
            .await?
            .unwrap_or_else(crate::progress_controller::ExecutionState::new);

            if post_state.repair.infra_transient {
                crate::state_manager::apply_execution_event(
                    &params.thread_store.control_store(),
                    params.thread_id,
                    crate::progress_controller::DataEngineerEvent::InfraTransientCleared,
                )
                .await
                .map_err(|e| format!("failed to persist infra-transient flag clear: {e}"))?;
                return Ok(PhaseOutcome::stayed_waiting(
                    "batch failed with transient infrastructure error; will retry",
                ));
            }

            let mutation_advanced = post_state.repair.mutated_since_fail;
            if mutation_advanced {
                Ok(PhaseOutcome::stayed_with_progress(
                    "authoring turn made mutations at the step boundary; resetting budget",
                ))
            } else {
                Ok(PhaseOutcome::stayed_waiting(
                    "authoring turn ended at the single-step boundary without a committed transition",
                ))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Orchestrator: thin wrapper that delegates to the extracted functions above
// ---------------------------------------------------------------------------

impl DataEngineerSuite {
    pub(super) async fn execute_author_phase(
        thread_store: &ThreadStore,
        thread_id: &str,
        phase: crate::control_flow::Phase,
        question: &str,
        sctx: &SuiteCtx,
        execution_state: &crate::progress_controller::ExecutionState,
        _thread_state_step_count: usize,
        repair_ctx: &crate::progress_controller::RepairContext,
    ) -> Result<PhaseOutcome, PhaseError> {
        let adapter = crate::authoring_driver::adapter_for_phase(phase)
            .ok_or_else(|| format!("authoring adapter missing for phase '{}'", phase.as_str()))?;
        let track = if adapter.kind() == crate::authoring_driver::AuthoringKind::Cleanse {
            TrackKind::Cleanse
        } else {
            TrackKind::Model
        };

        let force_failed = matches!(
            execution_state.phase.transition.as_ref(),
            Some(crate::progress_controller::PhaseTransition::PrecheckFailed { .. })
                | Some(crate::progress_controller::PhaseTransition::ValidateExecutionFailed { .. })
                | Some(crate::progress_controller::PhaseTransition::ValidateContractError { .. })
                | Some(crate::progress_controller::PhaseTransition::ValidateFail { .. })
        );
        let last_validate_failed = execution_state.last_validate_failed() || force_failed;
        let mutated_since_fail = if force_failed {
            false
        } else {
            execution_state.repair.mutated_since_fail
        };
        let probe_status = execution_state.probe_requirement_status();
        let (probe_required, probe_satisfied) = match probe_status {
            crate::progress_controller::ProbeRequirementStatus::NotRequired => (false, true),
            crate::progress_controller::ProbeRequirementStatus::Required => (true, false),
            crate::progress_controller::ProbeRequirementStatus::Allowed => (true, true),
            crate::progress_controller::ProbeRequirementStatus::ExhaustedRequireMutation => {
                (false, true)
            }
        };
        let sys = crate::prompts::with_time_context(if track.is_cleanse() {
            prompts::cleanse_system_prompt()
        } else {
            prompts::model_system_prompt()
        });
        let mut actx = react_core::agent::AgentCtxBuilder::new(
            sctx.llm().clone(),
            sctx.storage().clone(),
            sctx.scope().clone(),
            sctx.keyspace().clone(),
            std::sync::Arc::new(InterruptOnlyPolicy),
        )
        .top_k(30)
        .per_step_timeout_secs(10)
        .max_steps(crate::env_util::AUTHOR_MAX_STEPS)
        .thread_id(thread_id.to_string())
        .trace_tx(sctx.trace_tx().clone())
        .agent_name(crate::env_util::DEFAULT_AGENT_NAME)
        .vector(sctx.vector().clone())
        .thread_store(thread_store.clone())
        .resolved_config(sctx.resolved_config().clone())
        .build();
        crate::ctx_ext::copy_capabilities_to_actx(sctx, &mut actx);

        let params = AuthorPhaseCtx {
            thread_store,
            thread_id,
            phase,
            track,
            execution_state,
            repair_ctx,
            last_validate_failed,
            _mutated_since_fail: mutated_since_fail,
            probe_required,
            probe_satisfied,
        };

        let (plan_context, plan_state) = if params.track.is_cleanse() {
            match load_cleanse_author_context(&params, &mut actx).await? {
                AuthorPlanLoadResult::EarlyReturn(outcome) => return Ok(outcome),
                AuthorPlanLoadResult::Ready {
                    plan_context,
                    plan_state,
                } => (plan_context, plan_state),
            }
        } else {
            match load_model_author_context(&params, &mut actx).await? {
                AuthorPlanLoadResult::EarlyReturn(outcome) => return Ok(outcome),
                AuthorPlanLoadResult::Ready {
                    plan_context,
                    plan_state,
                } => (plan_context, plan_state),
            }
        };

        let (registry, tools_card) =
            Self::build_tools_for_phase(params.phase, false, sctx, &plan_state, false)?;

        let author_prompt =
            build_author_prompt(&params, &actx, question, &plan_context, &plan_state).await?;

        let pre_mutation_epoch = 0u64; // legacy parameter — mutation tracking now uses repair.mutated_since_fail
        match Agent::run_until_block_non_interactive(
            &registry,
            &actx,
            &sys,
            &tools_card,
            &author_prompt.question,
            author_prompt.llm_options,
        )
        .await
        {
            Ok(outcome) => {
                handle_author_run_outcome(&params, &actx, sctx, pre_mutation_epoch, outcome).await
            }
            Err(e) => Err(e.to_string().into()),
        }
    }
}
