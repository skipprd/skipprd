use super::*;
use crate::data_engineer::phase_contract::{commit_phase_decision, PhaseDecision};
use crate::data_engineer::phase_plan_lifecycle::TrackPlanDoc;

fn actionable_review_plan_detail(
    plan_key: &str,
    plan_update_summary: serde_json::Value,
    entry_step_idx: Option<usize>,
) -> serde_json::Value {
    crate::data_engineer::phase_reason_detail::plan_actionable_auto_approved(
        plan_key,
        plan_update_summary,
        entry_step_idx,
    )
}

fn auto_approved_plan_detail(source: &str) -> serde_json::Value {
    crate::data_engineer::phase_reason_detail::plan_auto_approved(source)
}

impl DataEngineerSuite {
    pub(super) async fn execute_plan_phase(
        thread_store: &ThreadStore,
        thread_id: &str,
        phase: crate::data_engineer::control_flow::Phase,
        question: &str,
        sctx: &SuiteCtx,
        execution_state: &crate::data_engineer::progress_controller::ExecutionState,
        guard: &crate::data_engineer::control_flow::DerivedGuardState,
        allow_ask_approval: bool,
        thread_state_step_count: usize,
        last_validate_brief: &Option<String>,
        _last_validate_failed_models: &[crate::data_engineer::progress_controller::FailedModelRef],
    ) -> Result<PhaseExecutorOutcome, String> {

// Plan phases are read-only discovery + plan authoring. They persist an approved
// plan to storage and then drive the subsequent authoring phase deterministically.
let track = TrackKind::try_from_plan_phase(phase)?;
let is_cleanse = track.is_cleanse();
let actx = Self::plan_agent_ctx(thread_id, sctx);
let entered_from_actionable_review =
    execution_state.phase.phase_reason_code == Some(PhaseReasonCode::ReviewPatchPlan);
let actionable_review_entry_step_idx =
    entered_from_actionable_review.then_some(thread_state_step_count);
let prior_plan_for_update = if entered_from_actionable_review {
    match track {
        TrackKind::Cleanse => {
            crate::data_engineer::phase_plan_lifecycle::load_any_plan_for_spec::<crate::data_engineer::CleanseSpec>(&actx).await
        }
        TrackKind::Model => {
            crate::data_engineer::phase_plan_lifecycle::load_any_plan_for_spec::<crate::data_engineer::ModelSpec>(&actx).await
        }
    }
} else {
    None
};

// Agent-mode hard cutover: no ask/reply approval semantics.
// Plan transitions are deterministic and state-driven; free-form question text
// must never be interpreted as approve/reject in this mode.

// If an approved/draft plan already exists (oldest active plan for this thread), move forward.
if let Some(mut existing_plan) =
    match track {
        TrackKind::Cleanse => {
            crate::data_engineer::phase_plan_lifecycle::load_active_plan_for_spec::<crate::data_engineer::CleanseSpec>(&actx).await
        }
        TrackKind::Model => {
            crate::data_engineer::phase_plan_lifecycle::load_active_plan_for_spec::<crate::data_engineer::ModelSpec>(&actx).await
        }
    }
{
    let current_digest = match &existing_plan {
        TrackPlanDoc::Cleanse(plan) => stable_json_digest(plan),
        TrackPlanDoc::Model(plan) => stable_json_digest(plan),
    };
    if matches!(
        existing_plan.status(),
        crate::data_engineer::plan::PlanStatus::Approved
            | crate::data_engineer::plan::PlanStatus::Completed
    ) {
        if crate::data_engineer::phase_gate::patch_plan_intent_blocks_fast_forward(
            &execution_state,
            phase,
            existing_plan.plan_key(),
            current_digest.as_deref(),
        ) {
            // Review requested a plan patch; do not bypass plan regeneration via old approved/completed plan.
            return Ok(PhaseExecutorOutcome::Continue);
        }
        // Safety: an empty/invalid approved plan would cause authoring to fast-forward.
        if existing_plan.is_empty() {
            let plan_key = existing_plan.plan_key().to_string();
            existing_plan.set_status(crate::data_engineer::plan::PlanStatus::Cancelled);
            crate::data_engineer::phase_plan_lifecycle::save_plan(&actx, &existing_plan).await?;
            commit_phase_decision(
                &thread_store,
                thread_id,
                Some(phase),
                PhaseDecision::annotation(
                    phase,
                    Some(PhaseReasonCode::PlanInvalidEmpty),
                    Some(crate::data_engineer::phase_reason_detail::to_value(
                        &crate::data_engineer::phase_reason_detail::PlanInvalidEmptyDetail {
                            plan_key,
                            status: format!("{:?}", existing_plan.status()),
                            tasks_len: existing_plan.tasks_len(),
                            batches_len: existing_plan.batches_len(),
                        },
                    )),
                ),
            )
            .await?;
            return Ok(PhaseExecutorOutcome::Continue);
        }
        crate::data_engineer::state_manager::mutate_execution_state(
            &thread_store,
            thread_id,
            |es| es.clear_pending_loopback_intent(),
        )
        .await
        .map(|_| ())?;
        commit_phase_decision(
            &thread_store,
            thread_id,
            Some(phase),
            PhaseDecision::forward(
                track.author_phase(),
                Some(PhaseReasonCode::PlanAlreadyApproved),
                Some(plan_status_reason_detail(&existing_plan.status())),
            ),
        )
        .await?;
        return Ok(PhaseExecutorOutcome::Continue);
    }
    if existing_plan.status() == crate::data_engineer::plan::PlanStatus::Draft {
        let mut removed_non_raw = 0usize;
        if let TrackPlanDoc::Cleanse(plan) = &mut existing_plan {
            removed_non_raw = Self::enforce_cleanse_plan_raw_only(plan);
            if removed_non_raw > 0 {
                crate::data_engineer::phase_plan_lifecycle::save_plan(&actx, &existing_plan)
                    .await
                    .map_err(|e| {
                        format!(
                            "failed to persist cleanse draft after raw-only enforcement: {e}"
                        )
                    })?;
            }
        }
        if existing_plan.is_empty() {
            let plan_key = existing_plan.plan_key().to_string();
            existing_plan.set_status(crate::data_engineer::plan::PlanStatus::Cancelled);
            crate::data_engineer::phase_plan_lifecycle::save_plan(&actx, &existing_plan).await?;
            let detail = match existing_plan {
                TrackPlanDoc::Cleanse(_) => crate::data_engineer::phase_reason_detail::to_value(
                    &crate::data_engineer::phase_reason_detail::CleanseDraftUngroundedDetail {
                        plan_key,
                        reason: "draft_cleanse_plan_not_raw_grounded".to_string(),
                        removed_non_raw,
                    },
                ),
                TrackPlanDoc::Model(_) => crate::data_engineer::phase_reason_detail::to_value(
                    &crate::data_engineer::phase_reason_detail::PlanInvalidEmptyDetail {
                        plan_key,
                        status: format!("{:?}", existing_plan.status()),
                        tasks_len: existing_plan.tasks_len(),
                        batches_len: existing_plan.batches_len(),
                    },
                ),
            };
            commit_phase_decision(
                &thread_store,
                thread_id,
                Some(phase),
                PhaseDecision::annotation(phase, Some(PhaseReasonCode::PlanInvalidEmpty), Some(detail)),
            )
            .await?;
            return Ok(PhaseExecutorOutcome::Continue);
        }
        if entered_from_actionable_review {
            let detail = match &existing_plan {
                TrackPlanDoc::Cleanse(plan) => {
                    let plan_update = Self::plan_update_summary_cleanse(
                        match prior_plan_for_update.as_ref() {
                            Some(TrackPlanDoc::Cleanse(prior)) => Some(prior),
                            _ => None,
                        },
                        plan,
                        actionable_review_entry_step_idx,
                    );
                    actionable_review_plan_detail(
                        &plan.plan_key,
                        plan_update,
                        actionable_review_entry_step_idx,
                    )
                }
                TrackPlanDoc::Model(plan) => {
                    let plan_update = Self::plan_update_summary_model(
                        match prior_plan_for_update.as_ref() {
                            Some(TrackPlanDoc::Model(prior)) => Some(prior),
                            _ => None,
                        },
                        plan,
                        actionable_review_entry_step_idx,
                    );
                    actionable_review_plan_detail(
                        &plan.plan_key,
                        plan_update,
                        actionable_review_entry_step_idx,
                    )
                }
            };
            let advanced = match &existing_plan {
                TrackPlanDoc::Cleanse(_) => {
                    Self::approve_cleanse_plan_draft_and_advance(
                        &thread_store,
                        thread_id,
                        phase,
                        &actx,
                        thread_state_step_count,
                        PhaseReasonCode::PlanAutoApproved,
                        detail,
                    )
                    .await?
                }
                TrackPlanDoc::Model(_) => {
                    Self::approve_model_plan_draft_and_advance(
                        &thread_store,
                        thread_id,
                        phase,
                        &actx,
                        thread_state_step_count,
                        PhaseReasonCode::PlanAutoApproved,
                        detail,
                    )
                    .await?
                }
            };
            if advanced {
                return Ok(PhaseExecutorOutcome::Continue);
            }
        }
        let advanced = match existing_plan {
            TrackPlanDoc::Cleanse(_) => {
                Self::approve_cleanse_plan_draft_and_advance(
                    &thread_store,
                    thread_id,
                    phase,
                    &actx,
                    thread_state_step_count,
                    PhaseReasonCode::PlanAutoApproved,
                    auto_approved_plan_detail("existing_draft_plan"),
                )
                .await?
            }
            TrackPlanDoc::Model(_) => {
                Self::approve_model_plan_draft_and_advance(
                    &thread_store,
                    thread_id,
                    phase,
                    &actx,
                    thread_state_step_count,
                    PhaseReasonCode::PlanAutoApproved,
                    auto_approved_plan_detail("existing_draft_plan"),
                )
                .await?
            }
        };
        if advanced {
            return Ok(PhaseExecutorOutcome::Continue);
        }
        return Ok(PhaseExecutorOutcome::Continue);
    }
}

// Generate a new draft plan via LLM and then ask the user to approve it.
Self::ensure_catalog_bootstrap_semaphored(thread_id, sctx).await?;
// Deterministic bootstrap: ensure the plan phase ALWAYS has grounded context recorded
// in the thread history. This prevents LLM loops that repeatedly call file list
// and never reach sql_schema/evidence, which would trip the plan_grounding guard.
//
// Important: we do NOT decide the plan here; we just provide enough reality to
// ground the LLM's plan authoring.
let mut bootstrap_summary: Option<String> = None;
if execution_state.needs_plan_bootstrap(phase) {
        // Bootstrap calls are intentionally conservative: list models (may be empty),
        // read core config files, list datasets, and run a minimal probe on one table.
        let query = sctx
            .query
            .as_ref()
            .ok_or_else(|| "query provider missing".to_string())?
            .clone();
        let files_tool = tools::files_tool::FilesTool {
            datasets: sctx.datasets.clone(),
        };
        let sql_schema_tool = tools::sql_schema::SqlSchemaTool {
            query: query.clone(),
            datasets: sctx.datasets.clone(),
            catalog: sctx.catalog.clone(),
        };
        let sql_stats_tool = tools::sql_stats::SqlStatsTool {
            catalog: sctx.catalog.clone(),
            datasets: sctx.datasets.clone(),
        };
        let sql_sample_tool = tools::sql_sample::SqlSampleTool {
            query: query.clone(),
        };
        let run_sql_tool = tools::sql_run::SqlRunTool {
            query: query.clone(),
        };

        let tool_timeout = |name: &str| {
            actx.policy
                .timeout_for_tool(name)
                .unwrap_or(actx.per_step_timeout_secs)
        };
        let files_tool_timeout = tool_timeout("file");
        let sql_schema_timeout = tool_timeout("sql_schema");
        let sql_stats_timeout = tool_timeout("sql_stats");
        let sql_sample_timeout = tool_timeout("sql_sample");
        let run_sql_timeout = tool_timeout("run_sql");

        let models_list = control_flow::call_and_record_tool(
            &thread_store,
            thread_id,
            Some("agent".to_string()),
            &files_tool,
            serde_json::json!({"op":"list","prefix":"models/","limit":500}),
            &actx,
            files_tool_timeout,
        )
        .await;
        let _dbt_project = control_flow::call_and_record_tool(
            &thread_store,
            thread_id,
            Some("agent".to_string()),
            &files_tool,
            serde_json::json!({"op":"get","path":"dbt_project.yml","max_chars":4000}),
            &actx,
            files_tool_timeout,
        )
        .await;
        let _packages = control_flow::call_and_record_tool(
            &thread_store,
            thread_id,
            Some("agent".to_string()),
            &files_tool,
            serde_json::json!({"op":"get","path":"packages.yml","max_chars":4000}),
            &actx,
            files_tool_timeout,
        )
        .await;
        let _schema_yml = control_flow::call_and_record_tool(
            &thread_store,
            thread_id,
            Some("agent".to_string()),
            &files_tool,
            serde_json::json!({"op":"get","path":"models/schema.yml","max_chars":6000}),
            &actx,
            files_tool_timeout,
        )
        .await;

        let tables_obs = control_flow::call_and_record_tool(
            &thread_store,
            thread_id,
            Some("agent".to_string()),
            &sql_schema_tool,
            serde_json::json!({}),
            &actx,
            sql_schema_timeout,
        )
        .await;
        let tables: Vec<String> = tables_obs
            .get("tables")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let mut probed: Option<String> = None;
        let mut probed_field: Option<String> = None;
        let mut probe_ok = false;
        if let Some(first) = tables.first() {
            let (ok, field) = Self::run_deterministic_probe_for_table(
                &thread_store,
                thread_id,
                &actx,
                &sql_schema_tool,
                &sql_stats_tool,
                &sql_sample_tool,
                &run_sql_tool,
                first,
                sql_schema_timeout,
                sql_stats_timeout,
                sql_sample_timeout,
                run_sql_timeout,
            )
            .await;
            probed = Some(first.clone());
            probed_field = field;
            probe_ok = ok;
        }

        let model_count = models_list
            .get("items")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let models_list_ok = models_list
            .get("ok")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let tables_ok = tables_obs
            .get("ok")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mut head_tables = tables.clone();
        head_tables.truncate(10);
        let bootstrap_sufficient = models_list_ok && tables_ok;
        bootstrap_summary = Some(format!(
            "Deterministic bootstrap (suite-provided):\n- models/ listed: {} item(s)\n- models_list_ok: {}\n- sql_schema_ok: {}\n- tables discovered (head): {:?}\n- probed table for evidence: {:?}\n- probed field: {:?}\n- probe_ok: {}\n- bootstrap_sufficient: {}\n\nIf models/ is empty, that's OK; proceed using sql_schema discovery.",
            model_count,
            models_list_ok,
            tables_ok,
            head_tables,
            probed,
            probed_field,
            probe_ok,
            bootstrap_sufficient
        ));
        if bootstrap_sufficient {
            let mut st = execution_state.clone();
            st.mark_plan_bootstrap_done(phase);
            st.save(&thread_store, thread_id).await.map_err(|e| {
                format!("failed to persist plan bootstrap state: {e}")
            })?;
        }
}
let sys = crate::util::time_context::with_time_context(if is_cleanse {
    prompts::cleanse_plan_system_prompt()
} else {
    prompts::model_plan_system_prompt()
});
let manifest_retry_signal = if is_cleanse {
    crate::data_engineer::progress_controller::ManifestLookupState::default()
} else {
    execution_state.manifest.manifest_lookup.clone()
};
let (registry, tools_card) = Self::build_tools_for_phase(
    phase,
    &guard,
    allow_ask_approval,
    sctx,
    None,
    None,
    manifest_retry_signal.retry_suppressed,
)?;

let mut q = if is_cleanse {
    format!(
        "Create a SILVER/cleanse execution plan (batched in groups of 5).\n\nOriginal goal:\n{}\n",
        question
    )
} else {
    format!(
        "Create a GOLD/model execution plan (batched in groups of 5) based ONLY on existing silver models under models/staging/.\n\nOriginal goal:\n{}\n",
        question
    )
};
if !is_cleanse {
    if manifest_retry_signal.retry_suppressed {
        tracing::info!(
            "data_engineer: model_plan manifest lookup fallback enabled plan_manifest_lookup_unavailable_fallback=true manifest_retry_suppressed=true failure_signature={} repeated_failure_count={}",
            manifest_retry_signal
                .failure_signature
                .as_deref()
                .unwrap_or("unknown"),
            manifest_retry_signal.repeated_failure_count
        );
        q.push_str(
            "\n\nDETERMINISTIC FALLBACK MODE (manifest lookup unavailable):\n\
             - Repeated json_file manifest lookup failures were detected in this model_plan phase.\n\
             - Do NOT call json_file for manifest on this retry.\n\
             - Use deterministic fallback evidence only: file list/get + sql_schema + sql_stats/sql_sample/run_sql against concrete relations.\n\
             - Continue plan grounding with available evidence; do not stall on manifest access.\n",
        );
    } else if manifest_retry_signal.noncanonical_attempt_count > 0 {
        q.push_str(
            "\n\nManifest contract reminder:\n\
             - If you query manifest nodes, use ONLY json_file query path:\"target/manifest.json\" pointer:\"/nodes\".\n\
             - Do NOT use path:\"manifest.json\" or storage-key-like manifest paths.\n",
        );
    }
}

// Inject global preflight semantic context/audiences (if available).
// CRITICAL: global_semantic_context is intended for GOLD model planning only,
// not for SILVER/cleanse planning.
if !is_cleanse {
    let global_key = sctx.keyspace.semantic_key(
        &sctx.scope,
        react_core::providers::catalog::types::GLOBAL_SEMANTIC_DATASET_ID,
    );
    if let Ok(v) = sctx.storage.get_json(&global_key).await {
        q.push_str("\n\nIMMUTABLE CONTEXT (global_semantic_context):\n");
        q.push_str(
            &serde_json::to_string_pretty(&v)
                .unwrap_or_else(|_| "{}".to_string()),
        );
        q.push('\n');
    }
}
// If this plan phase was entered because review explicitly required a plan change,
// include that feedback verbatim to ground the new plan.
if execution_state.phase.phase_reason_code == Some(PhaseReasonCode::ReviewPatchPlan) {
    if let Some(key) = execution_state
        .phase
        .phase_reason_detail
        .as_ref()
        .and_then(|v| v.get("meta"))
        .and_then(|v| v.get("review_ref"))
        .and_then(|v| v.get("key"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        if let Ok(bytes) = actx.storage.get_bytes(&key).await {
            let txt = String::from_utf8_lossy(&bytes).to_string();
            if !txt.trim().is_empty() {
                q.push_str("\nReview feedback requiring plan change (incorporate into this plan):\n");
                q.push_str(txt.trim());
                q.push('\n');
            }
        }
    }
}
if let Some(ref brief) = last_validate_brief {
    q.push_str("\nLast dbt_validate error summary (if any):\n");
    q.push_str(brief);
    q.push('\n');
}
if let Some(bs) = bootstrap_summary.as_ref() {
    q.push_str("\n\n");
    q.push_str(bs);
    q.push('\n');
}
// Plan mode should be especially broad: include the full available relation list (bounded)
// so planning never needs to guess table names.
if let Some(ds) = sctx.datasets.as_ref() {
    if let Ok(items) = ds.list_datasets().await {
        let mut tables: Vec<String> =
            items.into_iter().map(|d| d.fqn()).collect();
        tables.sort();
        if !tables.is_empty() {
            q.push_str("\n\nIMMUTABLE FACTS (available_relations, bounded):\n");
            q.push_str(
                &serde_json::to_string_pretty(&serde_json::json!({
                    "tables": tables.into_iter().take(300).collect::<Vec<_>>()
                }))
                .unwrap_or_else(|_| "{}".to_string()),
            );
            q.push('\n');
        }
    }
}

let llm_options = if is_cleanse {
    Self::planning_llm_options(
        PlanningLlmProfile::DiscoveryCleanse,
        "data_engineer.cleanse_plan",
        None,
    )
} else {
    Self::planning_llm_options(
        PlanningLlmProfile::DiscoveryModel,
        "data_engineer.model_plan",
        None,
    )
};
match Agent::run_until_block_non_interactive(
    &registry,
    &actx,
    &sys,
    &tools_card,
    &q,
    llm_options,
)
.await
{
    Ok(RunOutcomeNonInteractive::Final {
        thread_id: _tid,
        result: _result,
    }) => {
        // Hard cutover: planning control flow does not re-read thread logs.
        // Bootstrap + deterministic catalog grounding are the stateful guarantees.

        let (design_memo, design_critique) =
            Self::produce_critiqued_design_memo(&actx, is_cleanse, &q).await?;
        if is_cleanse {
            let discovered_raw = Self::discovered_raw_relations_from_catalog(
                sctx.datasets.as_ref(),
            )
            .await;
            let skeleton =
                Self::deterministic_cleanse_skeleton_from_discovered_raw(
                    &discovered_raw,
                )?;
            let mut plan = Self::compile_cleanse_skeleton_plan(&skeleton);
            crate::data_engineer::plan::prune_cleanse_plan_to_grounded_raw_datasets(
                &mut plan,
                &discovered_raw,
            );
            // Planning/repair must not emit evidence; the deterministic runner adds it later.
            for t in plan.tasks.iter_mut() {
                for it in t.checklist.iter_mut() {
                    it.evidence.clear();
                }
            }
            let pre_raw_ids: Vec<String> = plan.tasks.iter().map(|t| t.dataset_id.clone()).collect();
            let removed_non_raw_initial =
                Self::enforce_cleanse_plan_raw_only(&mut plan);
            if removed_non_raw_initial > 0 {
                let post_raw_ids: Vec<String> = plan.tasks.iter().map(|t| t.dataset_id.clone()).collect();
                tracing::warn!(
                    "enforce_cleanse_plan_raw_only: removed {} non-raw tasks. before={:?}, after={:?}",
                    removed_non_raw_initial,
                    pre_raw_ids,
                    post_raw_ids,
                );
                Self::push_snapshot_array_event(
                    &mut plan.project_snapshot,
                    "deterministic_plan_repairs",
                    serde_json::json!({
                        "kind": "cleanse_plan_raw_only_enforcement",
                        "removed_task_count": removed_non_raw_initial,
                    }),
                    200,
                );
            }
            plan.status = crate::data_engineer::plan::PlanStatus::Draft;
            plan.plan_key =
                crate::data_engineer::plan::new_cleanse_plan_key(&actx);
            // Hard cutover: do not persist pre-grounded draft plans.
            // Persistence begins only after grounding + semantic gates.
            // Scope progress to the current plan instance so we don't replay the full
            // historical log and accidentally mark tasks done from prior cycles.
            plan.progress.last_applied_step_idx = thread_state_step_count;
            // Capture a cheap snapshot for “current project as-is” provenance.
            plan.project_snapshot = serde_json::json!({
                "dbt_prefix": actx.keyspace.dbt_prefix(&actx.scope),
                "dbt_project_yml_etag": actx.storage.head_etag(&actx.keyspace.dbt_project_key(&actx.scope)).await.ok().flatten(),
                "plan_design_memo": Self::excerpt(&design_memo, 12_000),
                "plan_design_critique": {
                    "ok": design_critique.ok,
                    "blockers": design_critique.blockers.clone(),
                    "fixes": design_critique.fixes.clone()
                },
            });
            if entered_from_actionable_review {
                if let Some(obj) = plan.project_snapshot.as_object_mut() {
                    obj.insert(
                        "entry_reason_code".to_string(),
                        serde_json::json!("review_actionable_true"),
                    );
                    if let Some(idx) = actionable_review_entry_step_idx {
                        obj.insert(
                            "entry_step_idx".to_string(),
                            serde_json::json!(idx),
                        );
                    }
                }
            }
            // Hard cutover: schema facts for planning come from deterministic catalog artifacts,
            // not from replaying thread-log tool history.
            // Ground the plan against reality: only keep datasets we can prove exist via schema().
            let candidates =
                Self::collect_cleanse_grounding_candidates(&plan, &discovered_raw);
            if plan.tasks.is_empty() && !discovered_raw.is_empty() {
                tracing::warn!(
                    "cleanse plan skeleton produced 0 task ids; seeding grounding candidates from discovered_raw={:?}",
                    discovered_raw
                );
            }
            tracing::info!(
                "cleanse plan grounding: {} candidate dataset(s) before schema validation: {:?}",
                candidates.len(),
                candidates
            );
            let grounded =
                crate::data_engineer::dataset_truth::build_grounded_raw_dataset_set(&actx, &actx.warehouse, &candidates)
                    .await;
            if !grounded.rejected.is_empty() {
                for rej in grounded.rejected.iter() {
                    tracing::warn!(
                        "cleanse plan grounding: rejected dataset_id={:?} reason={:?}",
                        rej.dataset_id,
                        rej.reason
                    );
                }
            }
            tracing::info!(
                "cleanse plan grounding: {} allowed, {} rejected",
                grounded.allowed.len(),
                grounded.rejected.len()
            );
            crate::data_engineer::plan::prune_cleanse_plan_to_grounded_raw_datasets(
                &mut plan,
                &grounded.allowed,
            );
            if plan.tasks.is_empty() || plan.batches.is_empty() {
                if Self::synthesize_cleanse_plan_from_grounded_raw(
                    &mut plan,
                    &grounded.allowed,
                ) {
                    Self::push_snapshot_array_event(
                        &mut plan.project_snapshot,
                        "deterministic_plan_repairs",
                        serde_json::json!({
                            "kind": "cleanse_plan_grounding_empty_after_prune",
                            "action": "synthesized_from_grounded_raw",
                            "dataset_count": plan.tasks.len(),
                        }),
                        200,
                    );
                } else {
                    return Err(format!(
                        "cleanse plan grounding failed: 0 datasets survived. \
                        candidates={:?}, allowed={:?}, rejected=[{}]",
                        grounded.candidates,
                        grounded.allowed,
                        grounded.rejected.iter()
                            .map(|r| format!("{}:{}", r.dataset_id, r.reason))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            }
            let enrich_ids: Vec<String> = plan
                .tasks
                .iter()
                .map(|t| t.dataset_id.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            Self::enrich_cleanse_tasks(
                &actx,
                &q,
                &design_memo,
                &design_critique,
                &mut plan,
                &enrich_ids,
            )
            .await?;
            // Crash-safety: persist the grounded/pruned draft so resume/inspection reflects
            // what we actually validated/critiqued (not just the initial parsed JSON).
            crate::data_engineer::plan::save_cleanse_plan_grounded(
                &actx,
                &plan,
                Some(&grounded.allowed),
            )
                .await
                .map_err(|e| {
                    format!(
                        "failed to checkpoint grounded/pruned cleanse draft plan: {e}"
                    )
                })?;

            // Quality gate 1: semantic validity (includes implementation_spec requirements).
            // Hard cutover: normalize conservative defaults before validating (no LLM repair).
            let sem = crate::data_engineer::plan::ensure_cleanse_plan_semantically_valid_or_repaired(
                &actx,
                &mut plan,
            )
            .await?;
            let sem = if sem.ok {
                sem
            } else {
                let candidates: Vec<String> =
                    plan.tasks.iter().map(|t| t.dataset_id.clone()).collect();
                let targeted = Self::collect_targeted_semantic_tasks(
                    &sem.issues,
                    &candidates,
                );
                if !targeted.is_empty() {
                    Self::enrich_cleanse_tasks(
                        &actx,
                        &q,
                        &design_memo,
                        &design_critique,
                        &mut plan,
                        &targeted,
                    )
                    .await?;
                    crate::data_engineer::plan::ensure_cleanse_plan_semantically_valid_or_repaired(
                        &actx,
                        &mut plan,
                    )
                    .await?
                } else {
                    sem
                }
            };
            // Crash-safety: persist the normalized draft so resume/inspection reflects
            // what we actually validated (not just the initial parsed JSON).
            crate::data_engineer::plan::save_cleanse_plan_grounded(
                &actx,
                &plan,
                Some(&grounded.allowed),
            )
                .await
                .map_err(|e| {
                    format!(
                        "failed to checkpoint normalized cleanse draft plan: {e}"
                    )
                })?;
            if !sem.ok {
                let sem_errors = sem.messages();
                let reason = format!(
                    "Plan failed semantic validation (design-first). Errors:\n- {}",
                    sem_errors.join("\n- ")
                );
                apply_guard_block(
                    &thread_store,
                    thread_id,
                    phase,
                    GuardBlockKind::PlanSemanticInvalid,
                    reason.clone(),
                )
                .await?;
                // Hard cutover: same-phase blocks are represented as GuardBlock only.
                let tries = Self::bump_subjective_retry(
                    &thread_store,
                    thread_id,
                    phase,
                    crate::data_engineer::progress_controller::SubjectiveRetryKind::PlanSemanticInvalid,
                )
                .await;
                if tries
                    > crate::data_engineer::controller_kernel::subjective_retry_limit()
                {
                    return Err(format!(
                        "plan_semantic_validation_not_converged_after_retries: tries={}, errors={}",
                        tries,
                        sem.messages().join(" | ")
                    ));
                }
                return Ok(PhaseExecutorOutcome::Continue);
            }

            // Persist design-review details from the critique pass for downstream review/UI.
            if plan.project_snapshot.is_null() {
                plan.project_snapshot = serde_json::json!({});
            }
            if let Some(obj) = plan.project_snapshot.as_object_mut() {
                obj.insert(
                    "plan_design_review".to_string(),
                    serde_json::json!({
                        "ok": design_critique.ok,
                        "kind": "cleanse_plan",
                        "blockers": design_critique.blockers,
                        "fixes": design_critique.fixes,
                        "ts": chrono::Utc::now().to_rfc3339(),
                    }),
                );
            }

            crate::data_engineer::plan::save_cleanse_plan_grounded(
                &actx,
                &plan,
                Some(&grounded.allowed),
            )
            .await?;
            if Self::should_reset_subjective_retry_after_plan_save(
                entered_from_actionable_review,
            ) {
                Self::reset_subjective_retry(&thread_store, thread_id).await;
            }
            if entered_from_actionable_review {
                let plan_update = Self::plan_update_summary_cleanse(
                    match prior_plan_for_update.as_ref() {
                        Some(TrackPlanDoc::Cleanse(prior)) => Some(prior),
                        _ => None,
                    },
                    &plan,
                    actionable_review_entry_step_idx,
                );
                let detail = actionable_review_plan_detail(
                    &plan.plan_key,
                    plan_update,
                    actionable_review_entry_step_idx,
                );
                let advanced = Self::approve_cleanse_plan_draft_and_advance(
                    &thread_store,
                    thread_id,
                    phase,
                    &actx,
                    thread_state_step_count,
                    PhaseReasonCode::PlanAutoApproved,
                    detail,
                )
                .await?;
                if advanced {
                    return Ok(PhaseExecutorOutcome::Continue);
                }
            }
            let advanced = Self::approve_cleanse_plan_draft_and_advance(
                &thread_store,
                thread_id,
                phase,
                &actx,
                thread_state_step_count,
                PhaseReasonCode::PlanAutoApproved,
                auto_approved_plan_detail("new_draft_plan"),
            )
            .await?;
            if advanced {
                return Ok(PhaseExecutorOutcome::Continue);
            }
            return Ok(PhaseExecutorOutcome::Continue);
        } else {
            let candidates = Self::generate_model_candidates(
                &actx,
                &q,
                &design_memo,
                &design_critique,
            )
            .await?;
            let mut plan = Self::compile_model_candidates_plan(&candidates);
            // Planning/repair must not emit evidence; the deterministic runner adds it later.
            for t in plan.tasks.iter_mut() {
                for it in t.checklist.iter_mut() {
                    it.evidence.clear();
                }
            }
            let selected_candidates = Self::select_high_value_model_candidates(
                &candidates.candidates,
            );
            if plan.project_snapshot.is_null() {
                plan.project_snapshot = serde_json::json!({});
            }
            if let Some(obj) = plan.project_snapshot.as_object_mut() {
                obj.insert(
                    "model_candidate_selection".to_string(),
                    serde_json::json!({
                        "min_score": Self::model_plan_min_score(),
                        "candidate_count": candidates.candidates.len(),
                        "selected_count": selected_candidates.len(),
                        "selected": selected_candidates,
                    }),
                );
            }
            plan.status = crate::data_engineer::plan::PlanStatus::Draft;
            plan.plan_key =
                crate::data_engineer::plan::new_model_plan_key(&actx);
            // Hard cutover: do not persist pre-grounded draft plans.
            // Persistence begins only after grounding + semantic gates.
            // Scope progress to the current plan instance so we don't replay the full
            // historical log and accidentally mark tasks done from prior cycles.
            plan.progress.last_applied_step_idx = thread_state_step_count;
            plan.project_snapshot = serde_json::json!({
                "dbt_prefix": actx.keyspace.dbt_prefix(&actx.scope),
                "dbt_project_yml_etag": actx.storage.head_etag(&actx.keyspace.dbt_project_key(&actx.scope)).await.ok().flatten(),
                "plan_design_memo": Self::excerpt(&design_memo, 12_000),
                "plan_design_critique": {
                    "ok": design_critique.ok,
                    "blockers": design_critique.blockers.clone(),
                    "fixes": design_critique.fixes.clone()
                },
            });
            if entered_from_actionable_review {
                if let Some(obj) = plan.project_snapshot.as_object_mut() {
                    obj.insert(
                        "entry_reason_code".to_string(),
                        serde_json::json!("review_actionable_true"),
                    );
                    if let Some(idx) = actionable_review_entry_step_idx {
                        obj.insert(
                            "entry_step_idx".to_string(),
                            serde_json::json!(idx),
                        );
                    }
                }
            }
            // Ground gold planning: gold must be based ONLY on existing staging (silver) models.
            let stg = crate::data_engineer::dataset_truth::discover_staging_models_from_storage(&actx).await;
            crate::data_engineer::plan::prune_model_plan_to_grounded_staging_models(
                &mut plan,
                &stg.allowed_models,
            );
            if plan.tasks.is_empty() || plan.batches.is_empty() {
                // Hard cutover: same-phase blocks are represented as GuardBlock only.
                let tries = Self::bump_subjective_retry(
                    &thread_store,
                    thread_id,
                    phase,
                    crate::data_engineer::progress_controller::SubjectiveRetryKind::PlanGroundingEmptyAfterPrune,
                )
                .await;
                if tries
                    > crate::data_engineer::controller_kernel::subjective_retry_limit()
                {
                    return Err("model plan contained no grounded tasks after repeated retries (gold must reference existing silver models under models/staging/)".to_string());
                }
                return Ok(PhaseExecutorOutcome::Continue);
            }
            let enrich_ids: Vec<String> = plan
                .tasks
                .iter()
                .map(|t| t.name.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            Self::enrich_model_tasks(
                &actx,
                &q,
                &design_memo,
                &design_critique,
                &mut plan,
                &enrich_ids,
            )
            .await?;
            // Crash-safety: persist the grounded/pruned draft so resume/inspection reflects
            // what we actually validated/critiqued (not just the initial parsed JSON).
            crate::data_engineer::plan::save_model_plan_grounded(
                &actx,
                &plan,
                Some(&stg.allowed_models),
            )
            .await
            .map_err(|e| {
                format!(
                    "failed to checkpoint grounded/pruned model draft plan: {e}"
                )
            })?;

            // Quality gate 1: semantic validity (includes implementation_spec requirements).
            // Hard cutover: normalize conservative defaults before validating (no LLM repair).
            let sem = crate::data_engineer::plan::ensure_model_plan_semantically_valid_or_repaired(
                &actx,
                &mut plan,
                &stg.allowed_models,
            )
            .await?;
            let sem = if sem.ok {
                sem
            } else {
                let candidates: Vec<String> =
                    plan.tasks.iter().map(|t| t.name.clone()).collect();
                let targeted = Self::collect_targeted_semantic_tasks(
                    &sem.issues,
                    &candidates,
                );
                if !targeted.is_empty() {
                    Self::enrich_model_tasks(
                        &actx,
                        &q,
                        &design_memo,
                        &design_critique,
                        &mut plan,
                        &targeted,
                    )
                    .await?;
                    crate::data_engineer::plan::ensure_model_plan_semantically_valid_or_repaired(
                        &actx,
                        &mut plan,
                        &stg.allowed_models,
                    )
                    .await?
                } else {
                    sem
                }
            };
            // Crash-safety: persist the normalized draft so resume/inspection reflects
            // what we actually validated (not just the initial parsed JSON).
            crate::data_engineer::plan::save_model_plan_grounded(
                &actx,
                &plan,
                Some(&stg.allowed_models),
            )
            .await
            .map_err(|e| {
                format!(
                    "failed to checkpoint normalized model draft plan: {e}"
                )
            })?;
            if !sem.ok {
                let sem_errors = sem.messages();
                let reason = format!(
                    "Plan failed semantic validation (design-first). Errors:\n- {}",
                    sem_errors.join("\n- ")
                );
                apply_guard_block(
                    &thread_store,
                    thread_id,
                    phase,
                    GuardBlockKind::PlanSemanticInvalid,
                    reason.clone(),
                )
                .await?;
                // Hard cutover: same-phase blocks are represented as GuardBlock only.
                let tries = Self::bump_subjective_retry(
                    &thread_store,
                    thread_id,
                    phase,
                    crate::data_engineer::progress_controller::SubjectiveRetryKind::PlanSemanticInvalid,
                )
                .await;
                if tries
                    > crate::data_engineer::controller_kernel::subjective_retry_limit()
                {
                    return Err(format!(
                        "plan_semantic_validation_not_converged_after_retries: tries={}, errors={}",
                        tries,
                        sem.messages().join(" | ")
                    ));
                }
                return Ok(PhaseExecutorOutcome::Continue);
            }

            // Persist design-review details from the critique pass for downstream review/UI.
            if plan.project_snapshot.is_null() {
                plan.project_snapshot = serde_json::json!({});
            }
            if let Some(obj) = plan.project_snapshot.as_object_mut() {
                obj.insert(
                    "plan_design_review".to_string(),
                    serde_json::json!({
                        "ok": design_critique.ok,
                        "kind": "model_plan",
                        "blockers": design_critique.blockers,
                        "fixes": design_critique.fixes,
                        "ts": chrono::Utc::now().to_rfc3339(),
                    }),
                );
            }

            crate::data_engineer::plan::save_model_plan_grounded(
                &actx,
                &plan,
                Some(&stg.allowed_models),
            )
            .await?;
            if Self::should_reset_subjective_retry_after_plan_save(
                entered_from_actionable_review,
            ) {
                Self::reset_subjective_retry(&thread_store, thread_id).await;
            }
            if entered_from_actionable_review {
                let plan_update = Self::plan_update_summary_model(
                    match prior_plan_for_update.as_ref() {
                        Some(TrackPlanDoc::Model(prior)) => Some(prior),
                        _ => None,
                    },
                    &plan,
                    actionable_review_entry_step_idx,
                );
                let detail = actionable_review_plan_detail(
                    &plan.plan_key,
                    plan_update,
                    actionable_review_entry_step_idx,
                );
                let advanced = Self::approve_model_plan_draft_and_advance(
                    &thread_store,
                    thread_id,
                    phase,
                    &actx,
                    thread_state_step_count,
                    PhaseReasonCode::PlanAutoApproved,
                    detail,
                )
                .await?;
                if advanced {
                    return Ok(PhaseExecutorOutcome::Continue);
                }
            }
            let advanced = Self::approve_model_plan_draft_and_advance(
                &thread_store,
                thread_id,
                phase,
                &actx,
                thread_state_step_count,
                PhaseReasonCode::PlanAutoApproved,
                auto_approved_plan_detail("new_draft_plan"),
            )
            .await?;
            if advanced {
                return Ok(PhaseExecutorOutcome::Continue);
            }
            return Ok(PhaseExecutorOutcome::Continue);
        }
    }
    Ok(RunOutcomeNonInteractive::StepBoundary { .. }) => {
        // Deterministic single-step handoff: return to outer controller loop.
        return Ok(PhaseExecutorOutcome::Continue);
    }
    Err(e) => return Err(e),
}
                
    }
}
