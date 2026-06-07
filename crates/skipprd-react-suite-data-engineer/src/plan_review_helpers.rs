use super::*;
use crate::phase_contract::{commit_metered_decision, commit_phase_decision, PhaseDecision};
use react_core::storage::retry_list_prefix;

impl DataEngineerSuite {
    pub(super) fn collect_targeted_semantic_tasks(
        issues: &[crate::plan::PlanSemanticIssue],
        candidates: &[String],
    ) -> Vec<String> {
        let mut out = Vec::new();
        for issue in issues {
            if let Some(task_id) = issue.task_id.as_ref() {
                let key = task_id.trim();
                if !key.is_empty() && candidates.iter().any(|c| c == key) {
                    out.push(key.to_string());
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }

    pub(super) async fn approve_plan_draft_and_advance(
        thread_store: &ThreadStore,
        thread_id: &str,
        phase: control_flow::Phase,
        track: crate::track_spec::TrackKind,
        actx: &AgentCtx,
        log_len: usize,
        transition: crate::progress_controller::PhaseTransition,
    ) -> Result<bool, String> {
        use crate::phase_plan_lifecycle::{load_plan_for_track, save_plan, TrackPlanDoc};
        use crate::plan_types::TrackPlan;

        let Some(mut doc) = load_plan_for_track(actx, track).await else {
            return Ok(false);
        };
        if doc.status() != crate::plan::PlanStatus::Draft {
            return Ok(false);
        }

        // Pre-compute grounding once so we don't re-run discovery in the semantic check below.
        let staging_grounding = if matches!(&doc, TrackPlanDoc::Model(_)) {
            Some(crate::dataset_truth::discover_staging_models_from_storage(actx).await)
        } else {
            None
        };

        // Defensive grounding at approval time (facts can change; never assume).
        match &mut doc {
            TrackPlanDoc::Cleanse(p) => {
                let mut candidates: Vec<String> = Vec::new();
                for t in p.tasks.iter() {
                    if !t.dataset_id.trim().is_empty() {
                        candidates.push(t.dataset_id.trim().to_string());
                    }
                }
                for b in p.batches.iter() {
                    for ds in b.iter() {
                        if !ds.trim().is_empty() {
                            candidates.push(ds.trim().to_string());
                        }
                    }
                }
                candidates.sort();
                candidates.dedup();
                let wh = crate::ctx_ext::actx_warehouse(actx)
                    .ok_or_else(|| "warehouse provider missing for plan grounding".to_string())?;
                let grounded =
                    crate::dataset_truth::build_grounded_raw_dataset_set(actx, &wh, &candidates)
                        .await;
                crate::plan::prune_cleanse_plan_to_grounded_raw_datasets(p, &grounded.allowed);
            }
            TrackPlanDoc::Model(p) => {
                let stg = staging_grounding
                    .as_ref()
                    .expect("pre-computed for model track");
                crate::plan::prune_model_plan_to_grounded_staging_models(p, &stg.allowed_models);
            }
        }

        if doc.is_empty() {
            doc.cancel()
                .map_err(|e| format!("plan cancel failed: {e}"))?;
            save_plan(actx, &doc).await.map_err(|e| {
                format!(
                    "failed to persist pruned-empty {} plan: {e}",
                    track.as_str()
                )
            })?;
            commit_phase_decision(
                thread_store,
                thread_id,
                Some(phase),
                PhaseDecision::annotation(
                    phase,
                    Some(
                        crate::progress_controller::PhaseTransition::PlanPrunedEmpty {
                            plan_key: doc.plan_key().to_string(),
                        },
                    ),
                ),
            )
            .await?;
            return Ok(true);
        }

        doc.approve()
            .map_err(|e| format!("plan approval failed: {e}"))?;
        doc.progress_mut().last_applied_step_idx = log_len;
        save_plan(actx, &doc)
            .await
            .map_err(|e| format!("failed to persist approved {} plan: {e}", track.as_str()))?;
        crate::state_manager::apply_execution_event(
            &thread_store.control_store(),
            thread_id,
            crate::progress_controller::DataEngineerEvent::PatchImplIntentCleared,
        )
        .await
        .map_err(|e| format!("failed to clear pending patch impl intent: {e}"))?;

        let (tasks_count, batches_count) = match &doc {
            TrackPlanDoc::Cleanse(p) => (p.tasks.len() as u64, p.batches.len() as u64),
            TrackPlanDoc::Model(p) => (p.tasks.len() as u64, p.batches.len() as u64),
        };
        commit_metered_decision(
            thread_store,
            thread_id,
            Some(phase),
            PhaseDecision::forward(track.author_phase(), Some(transition)),
            vec![crate::metering::UsageEvent::PlanApproved {
                tasks: tasks_count,
                batches: batches_count,
                project_id: thread_id.to_string(),
            }],
            crate::metering::global_metering(),
        )
        .await?;
        Ok(true)
    }

    pub(super) async fn annotate_plan_semantic_invalid(
        thread_store: &ThreadStore,
        thread_id: &str,
        phase: control_flow::Phase,
        plan_key: &str,
        errors: &[String],
    ) -> Result<(), String> {
        commit_phase_decision(
            thread_store,
            thread_id,
            Some(phase),
            PhaseDecision::annotation(
                phase,
                Some(
                    crate::progress_controller::PhaseTransition::PlanSemanticInvalid {
                        plan_key: plan_key.to_string(),
                        errors: errors.to_vec(),
                    },
                ),
            ),
        )
        .await
    }

    pub(super) async fn authoring_complete_reason_detail(
        thread_store: &ThreadStore,
        thread_id: &str,
        has_proj: bool,
        has_models: bool,
    ) -> serde_json::Value {
        let execution_state = match crate::progress_controller::ExecutionState::load(
            &thread_store.control_store(),
            thread_id,
        )
        .await
        {
            Ok(Some(state)) => state,
            Ok(None) => crate::progress_controller::ExecutionState::new(),
            Err(e) => {
                tracing::error!(
                    thread_id = %thread_id,
                    error = %e,
                    "failed to load execution state for authoring completion detail"
                );
                crate::progress_controller::ExecutionState::new()
            }
        };
        let last_validate_failed = execution_state.last_validate_failed();
        let mutated_since_fail = execution_state.repair.mutated_since_fail;
        let probe_status = execution_state.probe_requirement_status();
        let (probe_required, probe_satisfied) = match probe_status {
            crate::progress_controller::ProbeRequirementStatus::NotRequired => (false, true),
            crate::progress_controller::ProbeRequirementStatus::Required => (true, false),
            crate::progress_controller::ProbeRequirementStatus::Allowed => (true, true),
            crate::progress_controller::ProbeRequirementStatus::ExhaustedRequireMutation => {
                (false, true)
            }
        };
        crate::phase_reason_detail::to_value(
            &crate::phase_reason_detail::AuthoringCompleteReasonDetail {
                invariants: crate::phase_reason_detail::AuthoringCompleteInvariantsDetail {
                    has_dbt_project_yml: has_proj,
                    has_any_models: has_models,
                },
                guard_state: crate::phase_reason_detail::AuthoringCompleteGuardStateDetail {
                    last_validate_failed,
                    mutated_since_fail,
                    probe_required,
                    probe_satisfied,
                },
            },
        )
    }
    pub(super) async fn has_any_gold_model_sql(actx: &AgentCtx) -> bool {
        let base = actx
            .keyspace()
            .scoped_prefix(actx.scope(), &["dbt"])
            .trim_end_matches('/')
            .to_string();
        let prefixes = [
            format!("{}/models/core/", base),
            format!("{}/models/marts/", base),
        ];
        for pref in prefixes.iter() {
            if let Ok(keys) = retry_list_prefix(actx.storage().as_ref(), pref).await {
                for k in keys {
                    if !k.ends_with(".sql") {
                        continue;
                    }
                    if k.contains("/_versions/") {
                        continue;
                    }
                    return true;
                }
            }
        }
        false
    }

    pub(super) fn strip_meta_line(answer: &str) -> String {
        let mut lines = answer.lines();
        let first = lines.next().unwrap_or("").trim();
        if first.starts_with("META:") {
            lines.collect::<Vec<&str>>().join("\n").trim().to_string()
        } else {
            answer.trim().to_string()
        }
    }

    pub(super) fn build_review_question_with_context(
        question: &str,
        phase: control_flow::Phase,
        execution_state: &crate::progress_controller::ExecutionState,
    ) -> String {
        let mut base = match phase {
            control_flow::Phase::CleanseReview => format!(
                "Conformance review of the DBT project after cleanse/staging work. Verify implementation matches the approved spec. Only flag correctness violations.\n\nOriginal goal:\n{}",
                question
            ),
            control_flow::Phase::ModelReview => format!(
                "Conformance review of the DBT project after modeling (core/gold) work. Verify implementation matches the approved spec. Only flag correctness violations.\n\nOriginal goal:\n{}",
                question
            ),
            _ => format!(
                "Final conformance review after publish. Only flag remaining correctness violations.\n\nOriginal goal:\n{}",
                question
            ),
        };

        let entry_transition = execution_state.phase.transition.as_ref();
        let entry_reason_detail: serde_json::Value = entry_transition
            .and_then(|t| serde_json::to_value(t).ok())
            .unwrap_or(serde_json::Value::Null);

        let mut prior_review_block: Option<String> = None;
        let is_re_review = matches!(
            entry_transition,
            Some(crate::progress_controller::PhaseTransition::ReviewProceed)
                | Some(crate::progress_controller::PhaseTransition::ReviewPatchImpl { .. })
        );
        if is_re_review {
            let parsed = serde_json::from_value::<
                crate::phase_reason_detail::ReviewDecisionTransitionDetail,
            >(entry_reason_detail.clone())
            .ok();
            let review_phase = parsed
                .as_ref()
                .map(|rd| rd.review_phase.as_str())
                .unwrap_or("unknown");
            let meta = parsed
                .as_ref()
                .and_then(|rd| serde_json::to_value(&rd.meta).ok())
                .unwrap_or(serde_json::Value::Null);
            let ans = parsed
                .as_ref()
                .and_then(|rd| serde_json::to_value(rd.answer).ok())
                .and_then(|v| v.as_str().map(|s| s.to_string()))
                .unwrap_or_default();
            let excerpt = {
                let cleaned = Self::strip_meta_line(&ans);
                let max = 700usize;
                if cleaned.len() > max {
                    format!("{}...", &cleaned[..max])
                } else {
                    cleaned
                }
            };
            let cycle_count = parsed
                .as_ref()
                .map(|rd| rd.review_subjective_retry_count)
                .unwrap_or(0);

            prior_review_block = Some(format!(
                "Previous review decision (this is RE-REVIEW cycle {cycle}):\n\
                 - review_phase: {review_phase}\n\
                 - meta: {meta}\n\
                 - excerpt: {excerpt}\n\
                 IMPORTANT: This code has already been reviewed and patched. Apply a HIGHER BAR for new findings. Only flag issues that are strictly new or represent a regression from the patch.",
                cycle = cycle_count + 1,
                review_phase = review_phase,
                meta = meta,
                excerpt = excerpt.replace('\n', " "),
            ));
        }

        let mut ctx_lines: Vec<String> = Vec::new();
        if let Some(prior) = prior_review_block {
            ctx_lines.push(prior);
        }
        if let Some(last_mutation) = execution_state.telemetry.last_mutation_summary.as_ref() {
            ctx_lines.push(format!(
                "Most recent mutation summary (state-derived):\n{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "op": last_mutation.op,
                    "affected_paths": last_mutation.affected_paths,
                    "select_terms": last_mutation.select_terms,
                }))
                .unwrap_or_else(|_| "{}".to_string())
            ));
        }
        if entry_transition.is_some() || !entry_reason_detail.is_null() {
            ctx_lines.push(format!(
                "Why we are reviewing now:\n- entry_reason_code: {}\n- entry_reason_detail: {}",
                entry_transition
                    .map(|t| t.as_reason_str())
                    .unwrap_or("null"),
                entry_reason_detail
            ));
        }

        if !ctx_lines.is_empty() {
            base = format!(
                "Review context (from thread history):\n{}\n\n{}",
                ctx_lines.join("\n\n"),
                base
            );
        }

        base
    }
}
