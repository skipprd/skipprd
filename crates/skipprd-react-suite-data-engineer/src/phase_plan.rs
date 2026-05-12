use super::*;
use crate::phase_contract::{commit_phase_decision, PhaseDecision};
use crate::phase_plan_lifecycle::TrackPlanDoc;
use crate::plan_progress::MAX_BATCH_SIZE;
use crate::plan_types::TrackPlan;
use react_core::keyspace::encode_key_component;
use react_core::storage::{retry_get_json, retry_head_etag};

const MODEL_PLAN_AMENDMENT_CHURN_WARNING: &str = "Preserve unaffected tasks exactly. Unnecessary byte diffs change contract digests and cause expensive re-authoring churn, so only surgically edit the named task IDs and explicitly required dependents.";

// ---------------------------------------------------------------------------
// Context struct: groups the recurring plan-phase parameters
// ---------------------------------------------------------------------------

struct PlanPhaseCtx<'a> {
    thread_store: &'a ThreadStore,
    thread_id: &'a str,
    phase: control_flow::Phase,
    track: TrackKind,
    actx: AgentCtx,
    thread_state_step_count: usize,
}

// ---------------------------------------------------------------------------
// Small, pre-existing helpers (unchanged)
// ---------------------------------------------------------------------------

async fn finalize_plan_and_approve(
    thread_store: &ThreadStore,
    thread_id: &str,
    phase: control_flow::Phase,
    track: TrackKind,
    actx: &AgentCtx,
    thread_state_step_count: usize,
    design_critique: &crate::plan_schema::PlanDesignCritiqueV1,
    critique_disposition: crate::enrichment::DesignCritiqueDisposition,
    sem: &crate::plan::PlanSemanticValidation,
    snapshot: &mut crate::plan_types::PlanSnapshot,
    retry_kinds: Vec<crate::progress_controller::SubjectiveRetryKind>,
) -> Result<PhaseOutcome, PhaseError> {
    if !sem.ok {
        return handle_plan_semantic_failure(sem, thread_store, thread_id, phase).await;
    }

    stamp_design_review(snapshot, track, design_critique, critique_disposition);

    DataEngineerSuite::clear_subjective_retries(thread_store, thread_id, retry_kinds).await?;

    let advanced = DataEngineerSuite::approve_plan_draft_and_advance(
        thread_store,
        thread_id,
        phase,
        track,
        actx,
        thread_state_step_count,
        crate::progress_controller::PhaseTransition::PlanAutoApproved {
            source: crate::progress_controller::AutoApprovalSource::SystemDefault,
        },
    )
    .await?;
    if advanced {
        Ok(PhaseOutcome::TransitionCommitted)
    } else {
        Ok(PhaseOutcome::stayed_waiting(
            "plan draft approval did not commit a phase transition",
        ))
    }
}

async fn load_active_plan_for_track_spec(
    actx: &AgentCtx,
    track: TrackKind,
) -> Option<TrackPlanDoc> {
    crate::phase_plan_lifecycle::load_plan_for_track(actx, track).await
}

/// Shared handler for plan semantic validation failure: emit guard block + check retry budget.
async fn handle_plan_semantic_failure(
    sem: &crate::plan::PlanSemanticValidation,
    thread_store: &ThreadStore,
    thread_id: &str,
    phase: control_flow::Phase,
) -> Result<PhaseOutcome, PhaseError> {
    let sem_errors = sem.messages();
    let reason = format!(
        "Plan failed semantic validation (design-first). Errors:\n- {}",
        sem_errors.join("\n- ")
    );
    apply_guard_block(
        thread_store,
        thread_id,
        phase,
        GuardBlockKind::PlanSemanticInvalid,
        reason,
    )
    .await?;
    use crate::retry_budget::SubjectiveRetryOutcome;
    match DataEngineerSuite::check_subjective_retry_budget(
        thread_store,
        thread_id,
        crate::progress_controller::SubjectiveRetryKind::PlanSemanticInvalid,
    )
    .await?
    {
        SubjectiveRetryOutcome::Exhausted(tries) => Err(format!(
            "plan_semantic_validation_not_converged_after_retries: tries={}, errors={}",
            tries,
            sem.messages().join(" | ")
        )
        .into()),
        SubjectiveRetryOutcome::WithinBudget(_) => Ok(PhaseOutcome::stayed_waiting(
            "plan semantic validation failed but remains within retry budget",
        )),
    }
}

/// Stamp design-review details from the critique pass into the plan's project_snapshot.
fn stamp_design_review(
    snapshot: &mut crate::plan_types::PlanSnapshot,
    track: TrackKind,
    critique: &crate::plan_schema::PlanDesignCritiqueV1,
    disposition: crate::enrichment::DesignCritiqueDisposition,
) {
    snapshot.insert(
        "plan_design_review",
        serde_json::json!({
            "ok": critique.ok,
            "disposition": disposition.as_str(),
            "kind": format!("{}_plan", track.as_str()),
            "blockers": critique.blockers,
            "fixes": critique.fixes,
            "ts": chrono::Utc::now().to_rfc3339(),
        }),
    );
}

// ---------------------------------------------------------------------------
// run_plan_bootstrap
// ---------------------------------------------------------------------------

struct PlanBootstrapOutcome {
    summary: Option<String>,
    /// True when deterministic bootstrap gathered sufficient evidence to skip
    /// the ReAct discovery loop (tables discovered, models/ empty).
    discovery_sufficient: bool,
}

async fn run_plan_bootstrap(
    thread_store: &ThreadStore,
    thread_id: &str,
    phase: crate::control_flow::Phase,
    sctx: &SuiteCtx,
    actx: &AgentCtx,
    execution_state: &crate::progress_controller::ExecutionState,
) -> Result<PlanBootstrapOutcome, String> {
    if !execution_state.needs_plan_bootstrap(phase) {
        return Ok(PlanBootstrapOutcome {
            summary: None,
            discovery_sufficient: false,
        });
    }
    let query =
        crate::ctx_ext::sctx_query(sctx).ok_or_else(|| "query provider missing".to_string())?;
    let files_tool = tools::files_tool::FilesTool {
        datasets: crate::ctx_ext::sctx_datasets(sctx),
    };
    let sql_schema_tool = tools::sql_schema::SqlSchemaTool {
        query: query.clone(),
        datasets: crate::ctx_ext::sctx_datasets(sctx),
        catalog: crate::ctx_ext::sctx_catalog(sctx),
    };
    let sql_stats_tool = tools::sql_stats::SqlStatsTool {
        catalog: crate::ctx_ext::sctx_catalog(sctx),
        datasets: crate::ctx_ext::sctx_datasets(sctx),
    };
    let sql_sample_tool = tools::sql_sample::SqlSampleTool {
        query: query.clone(),
    };
    let run_sql_tool = tools::sql_run::SqlRunTool {
        query: query.clone(),
    };

    let tool_timeout = |name: &str| {
        actx.policy()
            .timeout_for_tool(name)
            .unwrap_or(actx.per_step_timeout_secs())
    };
    let files_tool_timeout = tool_timeout("file");
    let sql_schema_timeout = tool_timeout("sql_schema");
    let sql_stats_timeout = tool_timeout("sql_stats");
    let sql_sample_timeout = tool_timeout("sql_sample");
    let run_sql_timeout = tool_timeout("run_sql");

    let models_list = control_flow::call_and_record_tool(
        thread_store,
        thread_id,
        Some(crate::env_util::DEFAULT_AGENT_NAME.to_string()),
        &files_tool,
        serde_json::json!({"op":"list","prefix":"models/","limit":crate::env_util::FILE_LIST_LIMIT}),
        actx,
        files_tool_timeout,
    )
    .await;
    let _dbt_project = control_flow::call_and_record_tool(
        thread_store,
        thread_id,
        Some(crate::env_util::DEFAULT_AGENT_NAME.to_string()),
        &files_tool,
        serde_json::json!({"op":"get","path":"dbt_project.yml","max_chars":4000}),
        actx,
        files_tool_timeout,
    )
    .await;
    let _packages = control_flow::call_and_record_tool(
        thread_store,
        thread_id,
        Some(crate::env_util::DEFAULT_AGENT_NAME.to_string()),
        &files_tool,
        serde_json::json!({"op":"get","path":"packages.yml","max_chars":4000}),
        actx,
        files_tool_timeout,
    )
    .await;
    let _schema_yml = control_flow::call_and_record_tool(
        thread_store,
        thread_id,
        Some(crate::env_util::DEFAULT_AGENT_NAME.to_string()),
        &files_tool,
        serde_json::json!({"op":"get","path":"models/schema.yml","max_chars":6000}),
        actx,
        files_tool_timeout,
    )
    .await;

    let tables_obs = control_flow::call_and_record_tool(
        thread_store,
        thread_id,
        Some(crate::env_util::DEFAULT_AGENT_NAME.to_string()),
        &sql_schema_tool,
        serde_json::json!({}),
        actx,
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
        let (ok, field) = DataEngineerSuite::run_deterministic_probe_for_table(
            thread_store,
            thread_id,
            actx,
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
    let profile_summary = match crate::ctx_ext::sctx_catalog(sctx) {
        Some(cat) => match cat
            .read_semantic_profile(sctx.scope(), crate::providers::GLOBAL_SEMANTIC_DATASET_ID)
            .await
        {
            Ok(Some(profile)) => {
                let dataset_count = profile.dataset_profiles.len();
                let observed_key_claims: usize = profile
                    .dataset_profiles
                    .iter()
                    .map(|d| {
                        d.key_candidates
                            .iter()
                            .filter(|k| k.status.authoring_safe())
                            .count()
                    })
                    .sum();
                let mut claim_heads = profile
                    .dataset_profiles
                    .iter()
                    .flat_map(|d| {
                        d.key_candidates.iter().map(move |k| {
                            format!(
                                "{{claim_id:{}, kind:candidate_key, status:{:?}, dataset:{}, fields:{:?}}}",
                                k.claim_id, k.status, d.dataset_id, k.field_names
                            )
                        })
                    })
                    .take(20)
                    .collect::<Vec<_>>();
                if claim_heads.is_empty() {
                    claim_heads.push("none".to_string());
                }
                format!(
                    "semantic_profile: available dataset_profiles={} observed_or_user_key_claims={} claim_refs_head=[{}]",
                    dataset_count,
                    observed_key_claims,
                    claim_heads.join("; ")
                )
            }
            Ok(None) => "semantic_profile: missing".to_string(),
            Err(e) => format!("semantic_profile: unavailable ({e})"),
        },
        None => "semantic_profile: catalog provider missing".to_string(),
    };
    let mut head_tables = tables.clone();
    head_tables.truncate(10);
    let bootstrap_sufficient = models_list_ok && tables_ok;
    let summary = format!(
        "Deterministic bootstrap (suite-provided):\n- models/ listed: {} item(s)\n- models_list_ok: {}\n- sql_schema_ok: {}\n- tables discovered (head): {:?}\n- probed table for evidence: {:?}\n- probed field: {:?}\n- probe_ok: {}\n- {}\n- bootstrap_sufficient: {}\n\nIf models/ is empty, that's OK; proceed using sql_schema discovery. Use only aggregate semantic_profile statuses/counts/claim refs as semantic evidence; never raw row values.",
        model_count,
        models_list_ok,
        tables_ok,
        head_tables,
        probed,
        probed_field,
        probe_ok,
        profile_summary,
        bootstrap_sufficient
    );
    if bootstrap_sufficient {
        crate::state_manager::apply_execution_event(
            &thread_store.control_store(),
            thread_id,
            crate::progress_controller::DataEngineerEvent::PlanBootstrapDone { phase },
        )
        .await
        .map_err(|e| format!("failed to persist plan bootstrap state: {e}"))?;
    }
    let discovery_sufficient = bootstrap_sufficient && model_count == 0;
    Ok(PlanBootstrapOutcome {
        summary: Some(summary),
        discovery_sufficient,
    })
}

// ---------------------------------------------------------------------------
// Extracted: consume_plan_revision
// ---------------------------------------------------------------------------

/// Load execution state, check for a pending plan-revision intent, consume it,
/// and — if the strategy is Rewrite — cancel the existing active plan.
/// Returns the violations list and whether this is a plan-revision entry.
async fn consume_plan_revision(
    pctx: &PlanPhaseCtx<'_>,
) -> Result<
    (
        Vec<crate::progress_controller::PlanViolation>,
        Option<crate::progress_controller::PlanRevisionStrategy>,
    ),
    PhaseError,
> {
    let plan_revision: Option<crate::progress_controller::PlanRevisionIntent> = {
        let es = crate::state_manager::load_execution_state_strict(
            &pctx.thread_store.control_store(),
            pctx.thread_id,
        )
        .await?
        .unwrap_or_else(crate::progress_controller::ExecutionState::new);
        let rev = es.phase.pending_plan_revision.clone();
        if rev.is_some() {
            crate::state_manager::apply_execution_event(
                &pctx.thread_store.control_store(),
                pctx.thread_id,
                crate::progress_controller::DataEngineerEvent::PlanRevisionConsumed,
            )
            .await
            .map_err(|e| {
                format!("failed to persist execution state after consuming plan revision: {e}")
            })?;
        }
        rev
    };
    let plan_violations: Vec<crate::progress_controller::PlanViolation> = plan_revision
        .as_ref()
        .map(|r| r.violations.clone())
        .unwrap_or_default();
    let strategy = plan_revision.as_ref().map(|r| r.strategy);

    if let Some(ref revision) = plan_revision {
        match revision.strategy {
            crate::progress_controller::PlanRevisionStrategy::Rewrite => {
                if let Some(mut existing_plan) =
                    load_active_plan_for_track_spec(&pctx.actx, pctx.track).await
                {
                    tracing::info!(
                        "data_engineer: cancelling plan '{}' for plan revision ({} violations)",
                        existing_plan.plan_key(),
                        plan_violations.len()
                    );
                    existing_plan.set_status(crate::plan::PlanStatus::Cancelled);
                    crate::phase_plan_lifecycle::save_plan(&pctx.actx, &existing_plan).await?;
                }
            }
            crate::progress_controller::PlanRevisionStrategy::Amend => {
                // Keep plan alive; violations will be injected into the LLM prompt
                // for targeted amendment.
            }
        }
    }

    Ok((plan_violations, strategy))
}

// ---------------------------------------------------------------------------
// Extracted: check_existing_plan
// ---------------------------------------------------------------------------

/// If an approved/draft plan already exists for this track, handle it and
/// return an outcome.  Returns `Ok(None)` when no existing plan is found
/// (or the existing plan doesn't warrant an early return).
async fn check_existing_plan(pctx: &PlanPhaseCtx<'_>) -> Result<Option<PhaseOutcome>, PhaseError> {
    let Some(mut existing_plan) = load_active_plan_for_track_spec(&pctx.actx, pctx.track).await
    else {
        return Ok(None);
    };

    if matches!(
        existing_plan.status(),
        crate::plan::PlanStatus::Approved | crate::plan::PlanStatus::Completed
    ) {
        if existing_plan.is_empty() {
            let plan_key = existing_plan.plan_key().to_string();
            existing_plan.set_status(crate::plan::PlanStatus::Cancelled);
            crate::phase_plan_lifecycle::save_plan(&pctx.actx, &existing_plan).await?;
            commit_phase_decision(
                pctx.thread_store,
                pctx.thread_id,
                Some(pctx.phase),
                PhaseDecision::annotation(
                    pctx.phase,
                    Some(
                        crate::progress_controller::PhaseTransition::PlanInvalidEmpty {
                            plan_key,
                            tasks_len: existing_plan.tasks_len(),
                            batches_len: existing_plan.batches_len(),
                        },
                    ),
                ),
            )
            .await?;
            return Ok(Some(PhaseOutcome::stayed_with_progress(
                "cancelled an empty approved plan and annotated the invalid state",
            )));
        }
        crate::state_manager::apply_execution_event(
            &pctx.thread_store.control_store(),
            pctx.thread_id,
            crate::progress_controller::DataEngineerEvent::PatchImplIntentCleared,
        )
        .await
        .map(|_| ())?;
        commit_phase_decision(
            pctx.thread_store,
            pctx.thread_id,
            Some(pctx.phase),
            PhaseDecision::forward(
                pctx.track.author_phase(),
                Some(crate::progress_controller::PhaseTransition::PlanAlreadyApproved),
            ),
        )
        .await?;
        return Ok(Some(PhaseOutcome::TransitionCommitted));
    }

    if existing_plan.status() == crate::plan::PlanStatus::Draft {
        if existing_plan.is_empty() {
            let plan_key = existing_plan.plan_key().to_string();
            existing_plan.set_status(crate::plan::PlanStatus::Cancelled);
            crate::phase_plan_lifecycle::save_plan(&pctx.actx, &existing_plan).await?;
            let _detail = match &existing_plan {
                TrackPlanDoc::Cleanse(_) => crate::phase_reason_detail::to_value(
                    &crate::phase_reason_detail::CleanseDraftUngroundedDetail {
                        plan_key: plan_key.clone(),
                        reason: "draft_cleanse_plan_empty".to_string(),
                        removed_non_raw: 0,
                    },
                ),
                TrackPlanDoc::Model(_) => crate::phase_reason_detail::to_value(
                    &crate::phase_reason_detail::PlanInvalidEmptyDetail {
                        plan_key: plan_key.clone(),
                        status: existing_plan.status(),
                        tasks_len: existing_plan.tasks_len(),
                        batches_len: existing_plan.batches_len(),
                    },
                ),
            };
            commit_phase_decision(
                pctx.thread_store,
                pctx.thread_id,
                Some(pctx.phase),
                PhaseDecision::annotation(
                    pctx.phase,
                    Some(
                        crate::progress_controller::PhaseTransition::PlanInvalidEmpty {
                            plan_key,
                            tasks_len: existing_plan.tasks_len(),
                            batches_len: existing_plan.batches_len(),
                        },
                    ),
                ),
            )
            .await?;
            return Ok(Some(PhaseOutcome::stayed_with_progress(
                "cancelled an empty draft plan and annotated the invalid state",
            )));
        }
        let advanced = DataEngineerSuite::approve_plan_draft_and_advance(
            pctx.thread_store,
            pctx.thread_id,
            pctx.phase,
            pctx.track,
            &pctx.actx,
            pctx.thread_state_step_count,
            crate::progress_controller::PhaseTransition::PlanAutoApproved {
                source: crate::progress_controller::AutoApprovalSource::SystemDefault,
            },
        )
        .await?;
        if advanced {
            return Ok(Some(PhaseOutcome::TransitionCommitted));
        }
        return Ok(Some(PhaseOutcome::stayed_waiting(
            "existing draft plan approval did not commit a phase transition",
        )));
    }

    Ok(None)
}

// ---------------------------------------------------------------------------
// Extracted: build_plan_query
// ---------------------------------------------------------------------------

/// Assemble the enriched question string sent to the planning agent, including
/// manifest-retry signals, global semantic context, repair context, violations,
/// bootstrap summary, available-relations facts, and authoritative column schemas.
///
/// All discovery data is read from the pre-built `PlanDiscoveryContext` — no
/// duplicate fetches. Returns (query_string, manifest_state).
async fn build_plan_query(
    pctx: &PlanPhaseCtx<'_>,
    sctx: &SuiteCtx,
    question: &str,
    execution_state: &crate::progress_controller::ExecutionState,
    repair_ctx: &crate::progress_controller::RepairContext,
    plan_violations: &[crate::progress_controller::PlanViolation],
    bootstrap_summary: Option<&str>,
    discovery: &crate::dataset_truth::PlanDiscoveryContext,
) -> (String, crate::progress_controller::ManifestLookupState) {
    let is_cleanse = pctx.track.is_cleanse();

    let manifest_retry_signal = if is_cleanse {
        crate::progress_controller::ManifestLookupState::default()
    } else {
        execution_state.manifest.manifest_lookup.clone()
    };

    let mut q = if is_cleanse {
        format!(
            "Create a SILVER/cleanse execution plan (batched in groups of {MAX_BATCH_SIZE}).\n\nOriginal goal:\n{}\n",
            question
        )
    } else {
        format!(
            "Create a GOLD/model execution plan based on existing silver models and any intra-plan gold dependencies.\n\
            Propose the canonical, highest-value gold models: entity dimension(s), process fact(s), \
            and a small set of the most aggregates. Do NOT produce every time-grain permutation, just the most valuable ones.\n\
            Models are executed in work-group batches of up to {MAX_BATCH_SIZE}.\n\n\
            Original goal:\n{}\n",
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
                 - Use deterministic fallback evidence only: file list/get + sql_schema + sql_stats/run_sql aggregate queries against concrete relations.\n\
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

    if !is_cleanse {
        let global_key = sctx.keyspace().scoped_key(
            sctx.scope(),
            &[
                "semantic",
                &format!(
                    "{}.yaml",
                    encode_key_component(crate::providers::GLOBAL_SEMANTIC_DATASET_ID)
                ),
            ],
        );
        if let Ok(v) = retry_get_json(sctx.storage().as_ref(), &global_key).await {
            q.push_str("\n\nIMMUTABLE CONTEXT (global_semantic_context):\n");
            q.push_str(&serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".to_string()));
            q.push('\n');
        }
    }

    if repair_ctx.has_context() {
        q.push_str("\n");
        q.push_str(&repair_ctx.format_error_context());
    }
    if !plan_violations.is_empty() {
        q.push_str("\n");
        q.push_str(&crate::progress_controller::format_plan_violations(
            plan_violations,
        ));
    }
    if let Some(bs) = bootstrap_summary {
        q.push_str("\n\n");
        q.push_str(bs);
        q.push('\n');
    }

    if !discovery.dataset_fqns.is_empty() {
        q.push_str("\n\nIMMUTABLE FACTS (available_relations, bounded):\n");
        q.push_str(
            &serde_json::to_string_pretty(&serde_json::json!({
                "tables": discovery.dataset_fqns.iter().take(300).collect::<Vec<_>>()
            }))
            .unwrap_or_else(|_| "{}".to_string()),
        );
        q.push('\n');
    }

    let schema_prompt_block =
        crate::dataset_truth::render_source_schema_prompt_block(&discovery.source_schemas);
    if !schema_prompt_block.is_empty() {
        q.push_str(&schema_prompt_block);
    }

    (q, manifest_retry_signal)
}

// ---------------------------------------------------------------------------
// Extracted: compile_and_ground_cleanse_plan
// ---------------------------------------------------------------------------

/// Deterministic cleanse-plan pipeline: skeleton → prune → ground → enrich →
/// validate → persist → finalize.
async fn compile_and_ground_cleanse_plan(
    pctx: &PlanPhaseCtx<'_>,
    _sctx: &SuiteCtx,
    q: &str,
    design_memo: &str,
    design_critique: &crate::plan_schema::PlanDesignCritiqueV1,
    critique_disposition: crate::enrichment::DesignCritiqueDisposition,
    discovery: &crate::dataset_truth::PlanDiscoveryContext,
) -> Result<PhaseOutcome, PhaseError> {
    let discovered_raw = &discovery.raw_dataset_ids;

    tracing::info!("data_engineer: [cleanse] compiling plan skeleton from discovered raw datasets");
    let skeleton =
        DataEngineerSuite::deterministic_cleanse_skeleton_from_discovered_raw(&discovered_raw)?;
    let mut plan = DataEngineerSuite::compile_cleanse_skeleton_plan(&skeleton);
    crate::plan::prune_cleanse_plan_to_grounded_raw_datasets(&mut plan, &discovered_raw);

    for t in plan.tasks.iter_mut() {
        for it in t.checklist.iter_mut() {
            it.evidence.clear();
        }
    }

    plan.status = crate::plan::PlanStatus::Draft;
    plan.plan_key = crate::plan::new_cleanse_plan_key(&pctx.actx);
    plan.progress.last_applied_step_idx = pctx.thread_state_step_count;
    let dbt_project_key = pctx
        .actx
        .keyspace()
        .scoped_key(pctx.actx.scope(), &["dbt", "dbt_project.yml"]);
    plan.project_snapshot = crate::plan_types::PlanSnapshot::from_value(serde_json::json!({
        "dbt_prefix": pctx.actx.keyspace().scoped_prefix(pctx.actx.scope(), &["dbt"]),
        "dbt_project_yml_etag": retry_head_etag(pctx.actx.storage().as_ref(), &dbt_project_key).await.ok().flatten(),
        "plan_design_memo": DataEngineerSuite::excerpt(design_memo, 12_000),
        "plan_design_critique": {
            "ok": design_critique.ok,
            "disposition": critique_disposition.as_str(),
            "blockers": design_critique.blockers.clone(),
            "fixes": design_critique.fixes.clone()
        },
    }));

    // Ground the plan against warehouse schema.
    tracing::info!("data_engineer: [cleanse] grounding plan against warehouse schema");
    let candidates =
        DataEngineerSuite::collect_cleanse_grounding_candidates(&plan, &discovered_raw);
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
    let grounded = crate::dataset_truth::build_grounded_raw_dataset_set(
        &pctx.actx,
        &crate::ctx_ext::actx_warehouse(&pctx.actx).ok_or_else(|| {
            "warehouse provider required for plan grounding but not configured".to_string()
        })?,
        &candidates,
    )
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
    crate::plan::prune_cleanse_plan_to_grounded_raw_datasets(&mut plan, &grounded.allowed);

    if plan.tasks.is_empty() || plan.batches.is_empty() {
        if DataEngineerSuite::synthesize_cleanse_plan_from_grounded_raw(
            &mut plan,
            &grounded.allowed,
        ) {
            DataEngineerSuite::push_snapshot_array_event(
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
                grounded
                    .rejected
                    .iter()
                    .map(|r| format!("{}:{}", r.dataset_id, r.reason))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
            .into());
        }
    }

    let enrich_ids: Vec<String> = plan
        .tasks
        .iter()
        .map(|t| t.dataset_id.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    DataEngineerSuite::enrich_cleanse_tasks(
        &pctx.actx,
        q,
        &discovery.source_schemas,
        design_memo,
        design_critique,
        &mut plan,
        &enrich_ids,
    )
    .await?;

    let _grounded_proof =
        crate::plan::save_cleanse_plan_grounded(&pctx.actx, &plan, &grounded.allowed)
            .await
            .map_err(|e| format!("failed to checkpoint grounded/pruned cleanse draft plan: {e}"))?;

    // Semantic validation (with one targeted-enrichment retry).
    tracing::info!("data_engineer: [cleanse] validating plan semantics");
    let sem = crate::plan::ensure_cleanse_plan_semantically_valid_or_repaired(&mut plan);
    let sem = if sem.ok {
        sem
    } else {
        let candidates: Vec<String> = plan.tasks.iter().map(|t| t.dataset_id.clone()).collect();
        let targeted = DataEngineerSuite::collect_targeted_semantic_tasks(&sem.issues, &candidates);
        if !targeted.is_empty() {
            DataEngineerSuite::enrich_cleanse_tasks(
                &pctx.actx,
                q,
                &discovery.source_schemas,
                design_memo,
                design_critique,
                &mut plan,
                &targeted,
            )
            .await?;
            crate::plan::ensure_cleanse_plan_semantically_valid_or_repaired(&mut plan)
        } else {
            sem
        }
    };

    let _grounded_proof =
        crate::plan::save_cleanse_plan_grounded(&pctx.actx, &plan, &grounded.allowed)
            .await
            .map_err(|e| format!("failed to checkpoint normalized cleanse draft plan: {e}"))?;

    use crate::progress_controller::SubjectiveRetryKind;
    finalize_plan_and_approve(
        pctx.thread_store,
        pctx.thread_id,
        pctx.phase,
        pctx.track,
        &pctx.actx,
        pctx.thread_state_step_count,
        design_critique,
        critique_disposition,
        &sem,
        &mut plan.project_snapshot,
        vec![SubjectiveRetryKind::PlanSemanticInvalid],
    )
    .await
}

// ---------------------------------------------------------------------------
// Extracted: compile_and_ground_model_plan
// ---------------------------------------------------------------------------

/// Deterministic model-plan pipeline: discover staging → generate candidates →
/// compile → prune → enrich → validate → persist → finalize.
///
/// `discovery.staging` must be `Some` before calling this function.
/// After enrichment populates `task.inputs`, this function queries the warehouse
/// for staging model output schemas and records them on each `ModelTask.source_schema`.
async fn compile_and_ground_model_plan(
    pctx: &PlanPhaseCtx<'_>,
    q: &str,
    design_memo: &str,
    design_critique: &crate::plan_schema::PlanDesignCritiqueV1,
    critique_disposition: crate::enrichment::DesignCritiqueDisposition,
    discovery: &crate::dataset_truth::PlanDiscoveryContext,
) -> Result<PhaseOutcome, PhaseError> {
    let staged = discovery
        .staging
        .as_ref()
        .expect("compile_and_ground_model_plan requires discovery.staging to be populated");
    if staged.allowed_models.is_empty() {
        use crate::retry_budget::SubjectiveRetryOutcome;
        match DataEngineerSuite::check_subjective_retry_budget(
            pctx.thread_store,
            pctx.thread_id,
            crate::progress_controller::SubjectiveRetryKind::PlanGroundingStagingDiscoveryEmpty,
        )
        .await?
        {
            SubjectiveRetryOutcome::Exhausted(tries) => {
                return Err(format!(
                    "no staging models discovered in storage after {} retries (expected stg_*.sql files under models/staging/); warnings: [{}]",
                    tries,
                    staged.warnings.join("; ")
                ).into());
            }
            SubjectiveRetryOutcome::WithinBudget(_) => {
                return Ok(PhaseOutcome::stayed_waiting(
                    "model planning is waiting for staging model discovery to yield grounded inputs",
                ));
            }
        }
    }

    tracing::info!("data_engineer: [model] compiling plan from candidate models");
    let candidates =
        DataEngineerSuite::generate_model_candidates(&pctx.actx, q, design_memo, design_critique)
            .await?;
    let mut plan = DataEngineerSuite::compile_model_candidates_plan(&candidates);

    for t in plan.tasks.iter_mut() {
        for it in t.checklist.iter_mut() {
            it.evidence.clear();
        }
    }

    let selected_candidates =
        DataEngineerSuite::select_high_value_model_candidates(&candidates.candidates);
    plan.project_snapshot.insert(
        "model_candidate_selection",
        serde_json::json!({
            "min_score": DataEngineerSuite::model_plan_min_score(),
            "candidate_count": candidates.candidates.len(),
            "selected_count": selected_candidates.len(),
            "selected": selected_candidates,
        }),
    );

    plan.status = crate::plan::PlanStatus::Draft;
    plan.plan_key = crate::plan::new_model_plan_key(&pctx.actx);
    plan.progress.last_applied_step_idx = pctx.thread_state_step_count;
    let dbt_project_key = pctx
        .actx
        .keyspace()
        .scoped_key(pctx.actx.scope(), &["dbt", "dbt_project.yml"]);
    plan.project_snapshot = crate::plan_types::PlanSnapshot::from_value(serde_json::json!({
        "dbt_prefix": pctx.actx.keyspace().scoped_prefix(pctx.actx.scope(), &["dbt"]),
        "dbt_project_yml_etag": retry_head_etag(pctx.actx.storage().as_ref(), &dbt_project_key).await.ok().flatten(),
        "plan_design_memo": DataEngineerSuite::excerpt(design_memo, 12_000),
        "plan_design_critique": {
            "ok": design_critique.ok,
            "disposition": critique_disposition.as_str(),
            "blockers": design_critique.blockers.clone(),
            "fixes": design_critique.fixes.clone()
        },
    }));

    // Enrich FIRST so the LLM populates task.inputs from the implementation spec,
    // then prune against allowed staging models. Candidates start with empty inputs;
    // enrichment is what grounds them to concrete staging refs.
    let enrich_ids: Vec<String> = plan
        .tasks
        .iter()
        .map(|t| t.name.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    DataEngineerSuite::enrich_model_tasks(
        &pctx.actx,
        q,
        &discovery.source_schemas,
        design_memo,
        design_critique,
        &mut plan,
        &enrich_ids,
    )
    .await?;

    // Record staging model output schemas on the plan. Enrichment has now
    // populated task.inputs with stg_* names, so we query the warehouse for
    // their output columns and stamp each ModelTask with both merged schemas
    // and grounded per-input relation facts.
    let mut staging_schemas = discovery.source_schemas.clone();
    let staging_prefix = crate::dataset_truth::staging_relation_prefix(&pctx.actx);
    let gold_prefix = crate::dataset_truth::gold_relation_prefix(&pctx.actx);
    {
        let all_stg_inputs: std::collections::BTreeSet<String> = plan
            .tasks
            .iter()
            .flat_map(|t| t.inputs.iter().map(|s| s.trim().to_string()))
            .filter(|s| !s.is_empty() && s.starts_with("stg_"))
            .collect();
        crate::dataset_truth::record_staging_output_schemas(
            &pctx.actx,
            &all_stg_inputs,
            &mut staging_schemas,
        )
        .await;
        for t in plan.tasks.iter_mut() {
            t.apply_source_schema_from(&staging_schemas);
            t.apply_grounded_inputs_from(&staging_schemas, staging_prefix.as_deref());
        }
        crate::plan_types::apply_intra_plan_grounded_inputs(
            &mut plan.tasks,
            gold_prefix.as_deref(),
        );
        let attached = crate::semantic_profile::attach_semantic_profile_claim_refs_to_model_plan(
            &pctx.actx, &mut plan,
        )
        .await?;
        tracing::info!(
            "data_engineer: [model] attached {} semantic_profile evidence claim ref(s)",
            attached
        );
    }

    tracing::info!("data_engineer: [model] grounding plan against existing staging models");
    crate::plan::prune_model_plan_to_grounded_staging_models(&mut plan, &staged.allowed_models);

    if plan.tasks.is_empty() || plan.batches.is_empty() {
        use crate::retry_budget::SubjectiveRetryOutcome;
        match DataEngineerSuite::check_subjective_retry_budget(
            pctx.thread_store,
            pctx.thread_id,
            crate::progress_controller::SubjectiveRetryKind::PlanGroundingEmptyAfterPrune,
        )
        .await?
        {
            SubjectiveRetryOutcome::Exhausted(tries) => {
                let allowed: Vec<&str> = staged.allowed_models.iter().map(|s| s.as_str()).collect();
                return Err(format!(
                    "model plan grounding pruned all tasks after {} retries; LLM candidates did not reference existing staging models. allowed_models={:?}",
                    tries, allowed
                ).into());
            }
            SubjectiveRetryOutcome::WithinBudget(_) => {
                return Ok(PhaseOutcome::stayed_waiting(
                    "model planning pruned to empty and is retrying within grounding budget",
                ));
            }
        }
    }

    // Semantic validation (with one targeted-enrichment retry).
    tracing::info!("data_engineer: [model] validating plan semantics");
    let sem = crate::plan::ensure_model_plan_semantically_valid_or_repaired(
        &mut plan,
        &staged.allowed_models,
    );
    let sem = if sem.ok {
        sem
    } else {
        let candidates: Vec<String> = plan.tasks.iter().map(|t| t.name.clone()).collect();
        let targeted = DataEngineerSuite::collect_targeted_semantic_tasks(&sem.issues, &candidates);
        if !targeted.is_empty() {
            DataEngineerSuite::enrich_model_tasks(
                &pctx.actx,
                q,
                &staging_schemas,
                design_memo,
                design_critique,
                &mut plan,
                &targeted,
            )
            .await?;
            for t in plan.tasks.iter_mut() {
                t.apply_source_schema_from(&staging_schemas);
                t.apply_grounded_inputs_from(&staging_schemas, staging_prefix.as_deref());
            }
            crate::plan_types::apply_intra_plan_grounded_inputs(
                &mut plan.tasks,
                gold_prefix.as_deref(),
            );
            crate::plan_types::reconcile_model_batches_and_work_groups(&mut plan);
            let attached =
                crate::semantic_profile::attach_semantic_profile_claim_refs_to_model_plan(
                    &pctx.actx, &mut plan,
                )
                .await?;
            tracing::info!(
                "data_engineer: [model] reattached {} semantic_profile evidence claim ref(s)",
                attached
            );
            crate::plan::ensure_model_plan_semantically_valid_or_repaired(
                &mut plan,
                &staged.allowed_models,
            )
        } else {
            sem
        }
    };

    if sem.ok {
        let _grounded_proof =
            crate::plan::save_model_plan_grounded(&pctx.actx, &plan, &staged.allowed_models)
                .await
                .map_err(|e| format!("failed to checkpoint normalized model plan: {e}"))?;
    }

    use crate::progress_controller::SubjectiveRetryKind;
    finalize_plan_and_approve(
        pctx.thread_store,
        pctx.thread_id,
        pctx.phase,
        pctx.track,
        &pctx.actx,
        pctx.thread_state_step_count,
        design_critique,
        critique_disposition,
        &sem,
        &mut plan.project_snapshot,
        vec![
            SubjectiveRetryKind::PlanSemanticInvalid,
            SubjectiveRetryKind::PlanGroundingEmptyAfterPrune,
            SubjectiveRetryKind::PlanGroundingStagingDiscoveryEmpty,
        ],
    )
    .await
}

fn normalize_model_revision_target(raw: &str, plan: &crate::plan::ModelPlan) -> Option<String> {
    let target = raw.trim().trim_start_matches("./");
    if target.is_empty() {
        return None;
    }
    for task in &plan.tasks {
        let name = task.name.trim();
        if target == name {
            return Some(name.to_string());
        }
        if let Some(path) = task.expected_model_path.as_deref() {
            let normalized_path = path.trim().trim_start_matches("./");
            if target == normalized_path {
                return Some(name.to_string());
            }
            let path_obj = std::path::Path::new(normalized_path);
            if path_obj.file_stem().and_then(|s| s.to_str()) == Some(target)
                || path_obj.file_name().and_then(|s| s.to_str()) == Some(target)
            {
                return Some(name.to_string());
            }
        }
    }
    None
}

fn targeted_model_revision_task_ids(
    violations: &[crate::progress_controller::PlanViolation],
    plan: &crate::plan::ModelPlan,
) -> std::collections::BTreeSet<String> {
    violations
        .iter()
        .filter_map(|violation| violation.task_id.as_deref())
        .filter_map(|target| normalize_model_revision_target(target, plan))
        .collect()
}

fn mark_model_amendment_targets_needs_update(
    plan: &mut crate::plan::ModelPlan,
    target_ids: &std::collections::BTreeSet<String>,
) {
    for task_id in target_ids {
        crate::plan::model_mark_needs_update(
            plan,
            task_id,
            Some("Plan amendment changed this task contract; re-author SQL against the amended spec."),
        );
        crate::plan::model_schema_contract_mark_needs_update(
            plan,
            task_id,
            Some("Plan amendment changed this task contract; re-author schema.yml against the amended spec."),
        );
    }
}

async fn amend_and_ground_model_plan(
    pctx: &PlanPhaseCtx<'_>,
    q: &str,
    design_memo: &str,
    design_critique: &crate::plan_schema::PlanDesignCritiqueV1,
    discovery: &crate::dataset_truth::PlanDiscoveryContext,
    violations: &[crate::progress_controller::PlanViolation],
) -> Result<PhaseOutcome, PhaseError> {
    let staged = discovery
        .staging
        .as_ref()
        .expect("amend_and_ground_model_plan requires discovery.staging to be populated");
    let mut plan = crate::plan::load_model_plan(&pctx.actx)
        .await?
        .ok_or_else(|| {
            "model plan amendment requested but no active model plan exists".to_string()
        })?;
    let before = plan.clone();
    let target_ids = targeted_model_revision_task_ids(violations, &plan);
    if target_ids.is_empty() {
        return Err("model plan amendment requested without targeted task_id(s); refusing full-plan regeneration to avoid contract churn".to_string().into());
    }

    let mut staging_schemas = discovery.source_schemas.clone();
    let existing_stg_inputs: std::collections::BTreeSet<String> = plan
        .tasks
        .iter()
        .flat_map(|t| t.inputs.iter().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty() && s.starts_with("stg_"))
        .collect();
    crate::dataset_truth::record_staging_output_schemas(
        &pctx.actx,
        &existing_stg_inputs,
        &mut staging_schemas,
    )
    .await;

    let amend_q = format!(
        "{q}\n\nAMENDMENT MODE:\n{MODEL_PLAN_AMENDMENT_CHURN_WARNING}\n\nTarget task_ids:\n{}\n",
        serde_json::to_string_pretty(&target_ids.iter().collect::<Vec<_>>())
            .unwrap_or_else(|_| "[]".to_string())
    );
    let target_vec: Vec<String> = target_ids.iter().cloned().collect();
    DataEngineerSuite::enrich_model_tasks(
        &pctx.actx,
        &amend_q,
        &staging_schemas,
        design_memo,
        design_critique,
        &mut plan,
        &target_vec,
    )
    .await?;

    let all_stg_inputs: std::collections::BTreeSet<String> = plan
        .tasks
        .iter()
        .flat_map(|t| t.inputs.iter().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty() && s.starts_with("stg_"))
        .collect();
    crate::dataset_truth::record_staging_output_schemas(
        &pctx.actx,
        &all_stg_inputs,
        &mut staging_schemas,
    )
    .await;
    let staging_prefix = crate::dataset_truth::staging_relation_prefix(&pctx.actx);
    let gold_prefix = crate::dataset_truth::gold_relation_prefix(&pctx.actx);
    for t in plan.tasks.iter_mut() {
        t.apply_source_schema_from(&staging_schemas);
        t.apply_grounded_inputs_from(&staging_schemas, staging_prefix.as_deref());
    }
    crate::plan_types::apply_intra_plan_grounded_inputs(&mut plan.tasks, gold_prefix.as_deref());
    let _ = crate::semantic_profile::attach_semantic_profile_claim_refs_to_model_plan(
        &pctx.actx, &mut plan,
    )
    .await?;

    crate::plan_diff::restore_unamended_model_task_contracts(&before, &mut plan, &target_ids);
    crate::plan_diff::guard_model_plan_amendment(&before, &plan, &target_ids)
        .map_err(PhaseError::ToolContractViolation)?;
    mark_model_amendment_targets_needs_update(&mut plan, &target_ids);
    crate::plan_types::reconcile_model_batches_and_work_groups(&mut plan);

    let sem = crate::plan::ensure_model_plan_semantically_valid_or_repaired(
        &mut plan,
        &staged.allowed_models,
    );
    if !sem.ok {
        return handle_plan_semantic_failure(&sem, pctx.thread_store, pctx.thread_id, pctx.phase)
            .await;
    }
    crate::plan::save_model_plan_grounded(&pctx.actx, &plan, &staged.allowed_models)
        .await
        .map_err(|e| format!("failed to checkpoint amended model plan: {e}"))?;
    DataEngineerSuite::clear_subjective_retries(
        pctx.thread_store,
        pctx.thread_id,
        vec![crate::progress_controller::SubjectiveRetryKind::PlanSemanticInvalid],
    )
    .await?;
    crate::phase_contract::commit_phase_decision(
        pctx.thread_store,
        pctx.thread_id,
        Some(pctx.phase),
        PhaseDecision::forward(
            pctx.track.author_phase(),
            Some(
                crate::progress_controller::PhaseTransition::PlanAutoApproved {
                    source: crate::progress_controller::AutoApprovalSource::SystemDefault,
                },
            ),
        ),
    )
    .await?;
    Ok(PhaseOutcome::TransitionCommitted)
}

fn deterministic_critiqued_design_memo(
    track: TrackKind,
    context: &str,
) -> crate::enrichment::CritiquedDesignMemo {
    let memo = format!(
        "Deterministic {} execution plan memo.\n\nContext summary:\n{}\n\nExecution requirements:\n- Build tasks from grounded source/staging inputs only.\n- Preserve the plan checklist/work-group execution model.\n- Let semantic validation reject unsupported evidence or incomplete specs.\n",
        track.as_str(),
        DataEngineerSuite::excerpt(context, 12_000)
    );
    crate::enrichment::CritiquedDesignMemo {
        memo,
        critique: crate::plan_schema::PlanDesignCritiqueV1 {
            ok: true,
            blockers: Vec::new(),
            fixes: Vec::new(),
        },
        disposition: crate::enrichment::DesignCritiqueDisposition::Accepted,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn model_plan_amendment_prompt_explains_byte_diff_churn() {
        assert!(super::MODEL_PLAN_AMENDMENT_CHURN_WARNING.contains("Unnecessary byte diffs"));
        assert!(super::MODEL_PLAN_AMENDMENT_CHURN_WARNING.contains("expensive re-authoring churn"));
        assert!(super::MODEL_PLAN_AMENDMENT_CHURN_WARNING.contains("surgically edit"));
    }
}

// ---------------------------------------------------------------------------
// Orchestrator
// ---------------------------------------------------------------------------

impl DataEngineerSuite {
    pub(super) async fn execute_plan_phase(
        thread_store: &ThreadStore,
        thread_id: &str,
        phase: crate::control_flow::Phase,
        question: &str,
        sctx: &SuiteCtx,
        execution_state: &crate::progress_controller::ExecutionState,
        thread_state_step_count: usize,
        repair_ctx: &crate::progress_controller::RepairContext,
    ) -> Result<PhaseOutcome, PhaseError> {
        let track = TrackKind::try_from_plan_phase(phase)?;
        let actx = Self::plan_agent_ctx(thread_id, sctx);
        let mut pctx = PlanPhaseCtx {
            thread_store,
            thread_id,
            phase,
            track,
            actx,
            thread_state_step_count,
        };

        // 1. Consume plan-revision intent (if any).
        let (plan_violations, plan_revision_strategy) = consume_plan_revision(&pctx).await?;
        let is_plan_revision = plan_revision_strategy.is_some();

        // 2. Fast-forward if a usable plan already exists.
        if !is_plan_revision {
            if let Some(outcome) = check_existing_plan(&pctx).await? {
                return Ok(outcome);
            }
        }

        // 3. Deterministic bootstrap.
        //    Catalog bootstrap is guaranteed by run_agent before the phase loop.
        let bootstrap = run_plan_bootstrap(
            thread_store,
            thread_id,
            phase,
            sctx,
            &pctx.actx,
            execution_state,
        )
        .await?;

        // 4. Single-discovery pass — all downstream functions read from this,
        //    no duplicate list_datasets / catalog / staging lookups.
        //    list_datasets() is already scoped to the configured source schema,
        //    so all returned tables are source/raw tables by definition.
        let mut discovery = {
            let mut dataset_fqns: Vec<String> = Vec::new();
            if let Some(ds) = crate::ctx_ext::sctx_datasets(sctx).as_ref() {
                if let Ok(items) = ds.list_datasets().await {
                    let mut tables: Vec<String> = items.into_iter().map(|d| d.fqn()).collect();
                    if pctx.track.is_cleanse() {
                        tables.retain(|fqn| {
                            crate::dataset_truth::is_cleanse_raw_source_dataset_candidate(fqn)
                        });
                    }
                    tables.sort();
                    dataset_fqns = tables;
                }
            }
            let raw_dataset_ids: std::collections::BTreeSet<String> =
                dataset_fqns.iter().cloned().collect();
            let (source_schemas, _prompt_block) =
                crate::dataset_truth::build_catalog_column_context(&pctx.actx, &dataset_fqns).await;
            crate::dataset_truth::PlanDiscoveryContext {
                dataset_fqns,
                raw_dataset_ids,
                source_schemas,
                staging: None,
            }
        };

        // 5. Build enriched query string from pre-built discovery context.
        let (q, manifest_retry_signal) = build_plan_query(
            &pctx,
            sctx,
            question,
            execution_state,
            repair_ctx,
            &plan_violations,
            bootstrap.summary.as_deref(),
            &discovery,
        )
        .await;

        // 6. For cleanse plans where the deterministic bootstrap already gathered
        //    all discoverable evidence (tables found, models/ empty), skip the
        //    ReAct discovery loop and proceed directly to plan compilation.
        if track.is_cleanse() && bootstrap.discovery_sufficient {
            tracing::info!(
                "data_engineer: deterministic bootstrap sufficient for cleanse plan, skipping ReAct discovery"
            );
            let critiqued = deterministic_critiqued_design_memo(track, &q);
            return compile_and_ground_cleanse_plan(
                &pctx,
                sctx,
                &q,
                &critiqued.memo,
                &critiqued.critique,
                critiqued.disposition,
                &discovery,
            )
            .await;
        }

        if !track.is_cleanse() {
            let mut staged =
                crate::dataset_truth::discover_staging_models_from_storage(&pctx.actx).await;
            if staged.allowed_models.is_empty() {
                for raw in &discovery.raw_dataset_ids {
                    let parts: Vec<&str> = raw.split('.').collect();
                    if parts.len() < 2 {
                        continue;
                    }
                    let schema = parts[parts.len() - 2];
                    let table = parts[parts.len() - 1];
                    let name = crate::naming::canonical_staging_model_name(schema, table);
                    staged.allowed_models.insert(name.clone());
                    staged.candidates.push(name);
                }
                if !staged.allowed_models.is_empty() {
                    tracing::warn!(
                        "data_engineer: synthesized {} staging model name(s) for model plan because storage staging discovery was empty",
                        staged.allowed_models.len()
                    );
                }
            }
            if !staged.allowed_models.is_empty() {
                tracing::info!(
                    "data_engineer: deterministic staging discovery sufficient for model plan ({} staging model(s)), skipping ReAct discovery",
                    staged.allowed_models.len()
                );
                let q_memo = crate::dataset_truth::enrich_query_with_staging_models(
                    &q,
                    &staged,
                    &discovery.source_schemas,
                );
                discovery.staging = Some(staged);
                let critiqued = deterministic_critiqued_design_memo(track, &q_memo);
                if plan_revision_strategy
                    == Some(crate::progress_controller::PlanRevisionStrategy::Amend)
                {
                    return amend_and_ground_model_plan(
                        &pctx,
                        &q_memo,
                        &critiqued.memo,
                        &critiqued.critique,
                        &discovery,
                        &plan_violations,
                    )
                    .await;
                }
                return compile_and_ground_model_plan(
                    &pctx,
                    &q_memo,
                    &critiqued.memo,
                    &critiqued.critique,
                    critiqued.disposition,
                    &discovery,
                )
                .await;
            }
        }

        let sys = crate::prompts::with_time_context(if track.is_cleanse() {
            prompts::cleanse_plan_system_prompt()
        } else {
            prompts::model_plan_system_prompt()
        });
        let (registry, tools_card) = Self::build_tools_for_phase(
            phase,
            false,
            sctx,
            &PlanState::ReadOnly,
            manifest_retry_signal.retry_suppressed,
        )?;
        let llm_options = if track.is_cleanse() {
            Self::planning_llm_options(
                PlanningLlmProfile::DiscoveryCleanse,
                "data_engineer.cleanse_plan",
                None,
            )?
        } else {
            Self::planning_llm_options(
                PlanningLlmProfile::DiscoveryModel,
                "data_engineer.model_plan",
                None,
            )?
        };

        // 7. Set step budget proportional to source count.
        let source_count = if track.is_cleanse() {
            discovery.raw_dataset_ids.len()
        } else {
            discovery.dataset_fqns.len()
        };
        pctx.actx
            .set_max_steps(crate::env_util::plan_discovery_steps_for_sources(
                source_count,
            ));
        tracing::info!(
            "data_engineer: plan discovery step budget = {} (source_count={})",
            pctx.actx.max_steps(),
            source_count,
        );

        // 8. Run the planning agent (full ReAct discovery).
        match Agent::run_until_block_non_interactive(
            &registry,
            &pctx.actx,
            &sys,
            &tools_card,
            &q,
            llm_options,
        )
        .await
        {
            Ok(RunOutcomeNonInteractive::Complete { .. }) => {
                tracing::info!(
                    "data_engineer: plan discovery complete for {}, beginning deterministic plan compilation",
                    track.as_str()
                );

                // For model plans, discover staging models ONCE (post-ReAct)
                // and enrich the planning context so the design memo references
                // the exact staging names.
                let q_memo = if !track.is_cleanse() {
                    let staged =
                        crate::dataset_truth::discover_staging_models_from_storage(&pctx.actx)
                            .await;
                    let enriched = crate::dataset_truth::enrich_query_with_staging_models(
                        &q,
                        &staged,
                        &discovery.source_schemas,
                    );
                    discovery.staging = Some(staged);
                    enriched
                } else {
                    q.clone()
                };

                let critiqued = deterministic_critiqued_design_memo(track, &q_memo);

                if track.is_cleanse() {
                    compile_and_ground_cleanse_plan(
                        &pctx,
                        sctx,
                        &q,
                        &critiqued.memo,
                        &critiqued.critique,
                        critiqued.disposition,
                        &discovery,
                    )
                    .await
                } else {
                    compile_and_ground_model_plan(
                        &pctx,
                        &q_memo,
                        &critiqued.memo,
                        &critiqued.critique,
                        critiqued.disposition,
                        &discovery,
                    )
                    .await
                }
            }
            Ok(RunOutcomeNonInteractive::StepBoundary { .. }) => {
                tracing::warn!(
                    "data_engineer: plan discovery hit step limit for {}; proceeding with evidence gathered so far",
                    track.as_str()
                );

                let q_memo = if !track.is_cleanse() {
                    let staged =
                        crate::dataset_truth::discover_staging_models_from_storage(&pctx.actx)
                            .await;
                    let enriched = crate::dataset_truth::enrich_query_with_staging_models(
                        &q,
                        &staged,
                        &discovery.source_schemas,
                    );
                    discovery.staging = Some(staged);
                    enriched
                } else {
                    q.clone()
                };

                let critiqued = deterministic_critiqued_design_memo(track, &q_memo);

                if track.is_cleanse() {
                    compile_and_ground_cleanse_plan(
                        &pctx,
                        sctx,
                        &q,
                        &critiqued.memo,
                        &critiqued.critique,
                        critiqued.disposition,
                        &discovery,
                    )
                    .await
                } else {
                    compile_and_ground_model_plan(
                        &pctx,
                        &q_memo,
                        &critiqued.memo,
                        &critiqued.critique,
                        critiqued.disposition,
                        &discovery,
                    )
                    .await
                }
            }
            Err(e) => Err(e.to_string().into()),
        }
    }
}
