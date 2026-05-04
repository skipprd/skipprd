use super::*;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DesignCritiqueDisposition {
    Accepted,
    AcceptedWithNovelBlockers,
    Rejected,
}

impl DesignCritiqueDisposition {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::AcceptedWithNovelBlockers => "accepted_with_novel_blockers",
            Self::Rejected => "rejected",
        }
    }
}

pub(super) struct CritiquedDesignMemo {
    pub memo: String,
    pub critique: crate::plan_schema::PlanDesignCritiqueV1,
    pub disposition: DesignCritiqueDisposition,
}

trait EnrichableTask: crate::plan::PlanTask + Sized {
    type Spec: serde::Serialize + DeserializeOwned;
    type EnrichmentItem;
    type EnrichmentResponse: DeserializeOwned + JsonSchema;

    fn track_kind() -> TrackKind;
    fn phase() -> control_flow::Phase;
    fn compile_schema_name() -> &'static str;
    fn reason_prompt_id() -> &'static str;
    fn compile_prompt_id() -> &'static str;
    fn retry_prompt_id() -> &'static str;
    fn enrichment_system_prompt() -> String;
    fn sanitizer_phase() -> &'static str;
    fn summarize_plan(plan: &crate::plan::Plan<Self>, max_lines: usize) -> String;
    fn response_items(response: Self::EnrichmentResponse) -> Vec<Self::EnrichmentItem>;
    fn item_parts(item: Self::EnrichmentItem) -> Result<(String, serde_json::Value), String>;
    fn apply_spec(task: &mut Self, spec: Self::Spec);
    fn apply_source_schema(task: &mut Self, schemas: &crate::plan_types::SourceSchema);
    fn is_placeholder(task: &Self) -> bool;
    fn retry_hint(failure_errors: &[String]) -> String;
}

fn resolve_design_critique_disposition(
    prior: &crate::plan_schema::PlanDesignCritiqueV1,
    revised: &crate::plan_schema::PlanDesignCritiqueV1,
) -> DesignCritiqueDisposition {
    if revised.ok {
        return DesignCritiqueDisposition::Accepted;
    }
    let prior_codes: std::collections::BTreeSet<_> =
        prior.blockers.iter().map(|b| b.code).collect();
    let has_overlap = revised
        .blockers
        .iter()
        .any(|b| prior_codes.contains(&b.code));
    if has_overlap {
        DesignCritiqueDisposition::Rejected
    } else {
        DesignCritiqueDisposition::AcceptedWithNovelBlockers
    }
}

impl EnrichableTask for crate::plan::CleanseTask {
    type Spec = crate::plan::CleanseImplementationSpec;
    type EnrichmentItem = crate::plan_schema::CleansePlanEnrichmentItemV1;
    type EnrichmentResponse = crate::plan_schema::CleansePlanEnrichmentV1;

    fn track_kind() -> TrackKind {
        TrackKind::Cleanse
    }

    fn phase() -> control_flow::Phase {
        control_flow::Phase::CleansePlan
    }

    fn compile_schema_name() -> &'static str {
        "suite.cleanse_plan_enrichment.v1"
    }

    fn reason_prompt_id() -> &'static str {
        "data_engineer.cleanse_plan_enrich_reason"
    }

    fn compile_prompt_id() -> &'static str {
        "data_engineer.cleanse_plan_enrich"
    }

    fn retry_prompt_id() -> &'static str {
        "data_engineer.cleanse_plan_enrich_retry"
    }

    fn enrichment_system_prompt() -> String {
        prompts::plan::cleanse_plan_enrichment_system_prompt()
    }

    fn sanitizer_phase() -> &'static str {
        "cleanse_enrich"
    }

    fn summarize_plan(plan: &crate::plan::Plan<Self>, max_lines: usize) -> String {
        crate::plan::summarize_cleanse_plan(plan, max_lines)
    }

    fn response_items(response: Self::EnrichmentResponse) -> Vec<Self::EnrichmentItem> {
        response.items
    }

    fn item_parts(item: Self::EnrichmentItem) -> Result<(String, serde_json::Value), String> {
        let spec_value =
            serde_json::to_value(&item.implementation_spec).map_err(|e| e.to_string())?;
        Ok((item.task_id, spec_value))
    }

    fn apply_spec(task: &mut Self, spec: Self::Spec) {
        task.implementation_spec = Some(spec);
    }

    fn apply_source_schema(task: &mut Self, schemas: &crate::plan_types::SourceSchema) {
        if let Some(cols) = schemas.get(task.dataset_id.trim()) {
            task.source_schema = cols.clone();
        }
    }

    fn is_placeholder(task: &Self) -> bool {
        task.implementation_spec.is_none()
    }

    fn retry_hint(failure_errors: &[String]) -> String {
        format!(
            "You previously returned invalid implementation_spec.\nErrors:\n{}\nOnly emit implementation_spec object with keys: spec_version,row_preserving,output_fields,prohibited_ops.\noutput_fields[].kind MUST be exactly one of: raw, clean, derived, quality_flag.\nEach output_fields item MUST include name, kind, expression.\nDo not use synonyms like passthrough/source/base/quality.\nNo wrappers, no extra fields.",
            failure_errors.join("\n")
        )
    }
}

impl EnrichableTask for crate::plan::ModelTask {
    type Spec = crate::plan::ModelImplementationSpec;
    type EnrichmentItem = crate::plan_schema::ModelPlanEnrichmentItemV1;
    type EnrichmentResponse = crate::plan_schema::ModelPlanEnrichmentV1;

    fn track_kind() -> TrackKind {
        TrackKind::Model
    }

    fn phase() -> control_flow::Phase {
        control_flow::Phase::ModelPlan
    }

    fn compile_schema_name() -> &'static str {
        "suite.model_plan_enrichment.v1"
    }

    fn reason_prompt_id() -> &'static str {
        "data_engineer.model_plan_enrich_reason"
    }

    fn compile_prompt_id() -> &'static str {
        "data_engineer.model_plan_enrich"
    }

    fn retry_prompt_id() -> &'static str {
        "data_engineer.model_plan_enrich_retry"
    }

    fn enrichment_system_prompt() -> String {
        prompts::plan::model_plan_enrichment_system_prompt()
    }

    fn sanitizer_phase() -> &'static str {
        "model_enrich"
    }

    fn summarize_plan(plan: &crate::plan::Plan<Self>, max_lines: usize) -> String {
        crate::plan::summarize_model_plan(plan, max_lines)
    }

    fn response_items(response: Self::EnrichmentResponse) -> Vec<Self::EnrichmentItem> {
        response.items
    }

    fn item_parts(item: Self::EnrichmentItem) -> Result<(String, serde_json::Value), String> {
        let spec_value =
            serde_json::to_value(&item.implementation_spec).map_err(|e| e.to_string())?;
        Ok((item.task_id, spec_value))
    }

    fn apply_spec(task: &mut Self, mut spec: Self::Spec) {
        let spec_inputs = spec.inputs.clone();
        spec.evidence_claim_refs.retain(|claim| {
            claim.status.authoring_safe()
                && crate::plan_validation::claim_ref_resolves_for_validation(claim)
        });
        task.implementation_spec = Some(spec);
        if !spec_inputs.is_empty() {
            task.inputs = spec_inputs;
        }
        if task.goal.trim().is_empty() {
            task.goal = format!("Build {} from grounded staging inputs.", task.name.trim());
        }
    }

    fn apply_source_schema(task: &mut Self, schemas: &crate::plan_types::SourceSchema) {
        let mut merged: Vec<crate::plan_types::SourceColumnDef> = Vec::new();
        for inp in &task.inputs {
            if let Some(cols) = schemas.get(inp.trim()) {
                merged.extend(cols.iter().cloned());
            }
        }
        if !merged.is_empty() {
            task.source_schema = merged;
        }
    }

    fn is_placeholder(task: &Self) -> bool {
        task.implementation_spec.is_none()
    }

    fn retry_hint(failure_errors: &[String]) -> String {
        format!(
            "You previously returned invalid implementation_spec.\nErrors:\n{}\nOnly emit implementation_spec object with keys: spec_version,grain,inputs,joins,metrics,output_fields,assumptions,evidence_claim_refs.\noutput_fields[].kind MUST be exactly one of: raw, clean, derived, quality_flag.\nEach output_fields item MUST include name, kind, expression. Each metrics[] item MUST include source_fields.\nDo not use synonyms like passthrough/source/base/quality.\nNo wrappers, no extra fields.",
            failure_errors.join("\n")
        )
    }
}

impl DataEngineerSuite {
    pub(super) fn plan_enrich_chunk_size() -> usize {
        env_util::plan_enrich_chunk_size()
    }

    fn apply_enrichment_items<T: EnrichableTask>(
        plan: &mut crate::plan::Plan<T>,
        allowed_task_ids: &[String],
        items: Vec<T::EnrichmentItem>,
    ) -> (Vec<String>, Vec<String>) {
        let mut failed: Vec<String> = Vec::new();
        let mut failure_errors: Vec<String> = Vec::new();
        for it in items {
            let (task_id, spec_value) = match T::item_parts(it) {
                Ok(parts) => parts,
                Err(e) => {
                    failure_errors.push(e);
                    continue;
                }
            };
            if !allowed_task_ids.iter().any(|t| t == &task_id) {
                continue;
            }
            match Self::parse_impl_spec_value_with_sanitize::<T::Spec>(spec_value, T::track_kind())
            {
                Ok((spec, stripped)) => {
                    if !stripped.is_empty() {
                        Self::push_snapshot_array_event(
                            &mut plan.project_snapshot,
                            "spec_sanitizer_events",
                            serde_json::json!({
                                "phase": T::sanitizer_phase(),
                                "task_id": task_id,
                                "stripped_keys": stripped
                            }),
                            200,
                        );
                    }
                    if let Some(task) = plan.tasks.iter_mut().find(|task| task.task_id() == task_id)
                    {
                        T::apply_spec(task, spec);
                    }
                }
                Err(e) => {
                    failed.push(task_id);
                    failure_errors.push(e);
                }
            }
        }
        (failed, failure_errors)
    }

    pub(super) fn build_enrichment_prompt_envelope(
        phase: control_flow::Phase,
        directive: crate::prompt_packets::TurnDirective,
        plan_kind: crate::plan_kind::PlanKind,
        plan_key: &str,
        planning_context: &str,
        memo: &str,
        critique: &crate::plan_schema::PlanDesignCritiqueV1,
        plan_summary: &str,
        task_ids: &[String],
        new_evidence_refs: &[String],
    ) -> String {
        let context_text = format!(
            "Context:\n{}\n\nDesign memo:\n{}\n\n{}\n\nCurrent plan summary:\n{}",
            Self::excerpt(planning_context, 20_000),
            Self::excerpt(memo, 12_000),
            Self::critique_guidance(critique),
            plan_summary
        );
        let envelope = crate::prompt_packets::PromptEnvelope {
            phase,
            goal: "Produce implementation_spec content for target tasks".to_string(),
            directive,
            plan: Some(crate::prompt_packets::PlanContextPacket {
                plan_kind: Some(plan_kind),
                plan_key: Some(plan_key.to_string()),
                context_text: Some(context_text),
                unresolved_ids: task_ids.to_vec(),
                new_evidence_refs: new_evidence_refs.to_vec(),
            }),
            batch: None,
        };
        crate::prompt_packets::render_envelope(&envelope).unwrap_or_else(|_| "{}".to_string())
    }

    pub(super) fn compile_prompt_from_reason(reason_memo: &str, base_user: &str) -> String {
        format!(
            "Reason memo:\n{}\n\n{}",
            Self::excerpt(reason_memo, 8_000),
            base_user
        )
    }

    /// Render a per-task schema block so the LLM sees each task's available
    /// columns directly adjacent to the task_id it is enriching.
    fn render_per_task_schema_block(
        task_ids: &[String],
        source_schemas: &crate::plan_types::SourceSchema,
    ) -> String {
        let mut out = String::new();
        for tid in task_ids {
            if let Some(cols) = source_schemas.get(tid.trim()) {
                if cols.is_empty() {
                    continue;
                }
                out.push_str(&format!(
                    "\nAVAILABLE COLUMNS FOR {} (source_columns MUST reference ONLY these):\n",
                    tid
                ));
                for c in cols {
                    out.push_str(&format!("  - {} ({})\n", c.name, c.data_type));
                }
            }
        }
        out
    }

    fn unresolved_enrichment_task_ids<T: EnrichableTask>(
        plan: &crate::plan::Plan<T>,
        task_ids: &[String],
    ) -> Vec<String> {
        task_ids
            .iter()
            .filter(|task_id| {
                plan.tasks
                    .iter()
                    .find(|task| task.task_id() == task_id.as_str())
                    .map(T::is_placeholder)
                    .unwrap_or(true)
            })
            .cloned()
            .collect()
    }

    async fn enrich_tasks<T: EnrichableTask>(
        ctx: &AgentCtx,
        planning_context: &str,
        source_schemas: &crate::plan_types::SourceSchema,
        memo: &str,
        critique: &crate::plan_schema::PlanDesignCritiqueV1,
        plan: &mut crate::plan::Plan<T>,
        task_ids: &[String],
    ) -> Result<(), String> {
        use react_core::llm::{ChatMessage, ChatRole};
        for chunk in task_ids.chunks(Self::plan_enrich_chunk_size()) {
            let chunk_vec = chunk.to_vec();
            let summary = T::summarize_plan(plan, 50);
            let per_task_schema = Self::render_per_task_schema_block(&chunk_vec, source_schemas);
            let base_user = format!(
                "{}\n\nTarget task_ids:\n{}\n{}\nReturn schema-valid enrichment JSON.",
                Self::build_enrichment_prompt_envelope(
                    T::phase(),
                    crate::prompt_packets::TurnDirective::Compile,
                    T::track_kind(),
                    &plan.plan_key,
                    planning_context,
                    memo,
                    critique,
                    &summary,
                    &chunk_vec,
                    &[],
                ),
                serde_json::to_string_pretty(&chunk_vec).unwrap_or_else(|_| "[]".to_string()),
                per_task_schema,
            );
            let reason_user = format!(
                "Think through the enrichment strategy for these task_ids. Return plain text only, no JSON.\n\n{}",
                Self::build_enrichment_prompt_envelope(
                    T::phase(),
                    crate::prompt_packets::TurnDirective::Reason,
                    T::track_kind(),
                    &plan.plan_key,
                    planning_context,
                    memo,
                    critique,
                    &summary,
                    &chunk_vec,
                    &[],
                )
            );
            let reason_memo = ctx
                .llm_chat(
                    &[
                        ChatMessage {
                            role: ChatRole::System,
                            content: prompts::plan::plan_enrichment_reason_system_prompt(),
                        },
                        ChatMessage {
                            role: ChatRole::User,
                            content: reason_user,
                        },
                    ],
                    &Self::planning_llm_options(
                        PlanningLlmProfile::EnrichmentReason,
                        T::reason_prompt_id(),
                        ctx.thread_id().clone(),
                    )?,
                )
                .await
                .map_err(|e| e.to_string())?;
            let compile_user = Self::compile_prompt_from_reason(&reason_memo, &base_user);
            let mut opts = Self::planning_llm_options(
                PlanningLlmProfile::EnrichmentCompile,
                T::compile_prompt_id(),
                ctx.thread_id().clone(),
            )?;
            opts.expected_format = react_core::llm::LlmExpectedFormat::JsonSchema(
                react_core::schema_registry::OpenAiStrictSchema::for_type::<T::EnrichmentResponse>(
                    T::compile_schema_name(),
                )
                .map_err(|e| e.to_string())?,
            );
            let raw = ctx
                .llm_chat(
                    &[
                        ChatMessage {
                            role: ChatRole::System,
                            content: T::enrichment_system_prompt(),
                        },
                        ChatMessage {
                            role: ChatRole::User,
                            content: compile_user,
                        },
                    ],
                    &opts,
                )
                .await
                .map_err(|e| e.to_string())?;
            let enrich = Self::parse_json_typed_strict::<T::EnrichmentResponse>(&raw)?;
            let (failed, failure_errors) =
                Self::apply_enrichment_items::<T>(plan, &chunk_vec, T::response_items(enrich));
            if !failed.is_empty() {
                let retry_hint = T::retry_hint(&failure_errors);
                let retry_user = format!(
                    "{}\n\nTarget task_ids:\n{}\n\nSTRICT RETRY REQUIREMENTS:\n{}\n\nReturn schema-valid enrichment JSON.",
                    Self::build_enrichment_prompt_envelope(
                        T::phase(),
                        crate::prompt_packets::TurnDirective::Verify,
                        T::track_kind(),
                        &plan.plan_key,
                        planning_context,
                        memo,
                        critique,
                        &T::summarize_plan(plan, 50),
                        &failed,
                        &failure_errors,
                    ),
                    serde_json::to_string_pretty(&failed).unwrap_or_else(|_| "[]".to_string()),
                    retry_hint
                );
                let retry_opts = react_core::llm::LlmCallOptions {
                    prompt_id: T::retry_prompt_id(),
                    ..opts
                };
                let retry_raw = ctx
                    .llm_chat(
                        &[
                            ChatMessage {
                                role: ChatRole::System,
                                content: T::enrichment_system_prompt(),
                            },
                            ChatMessage {
                                role: ChatRole::User,
                                content: retry_user,
                            },
                        ],
                        &retry_opts,
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                let retry_enrich =
                    Self::parse_json_typed_strict::<T::EnrichmentResponse>(&retry_raw)?;
                let (retry_failed, retry_errors) = Self::apply_enrichment_items::<T>(
                    plan,
                    &failed,
                    T::response_items(retry_enrich),
                );
                if !retry_failed.is_empty() {
                    return Err(format!(
                        "{} enrichment invalid after bounded retry for task_ids={}: {}",
                        T::track_kind().as_str(),
                        retry_failed.join(","),
                        retry_errors.join(" | ")
                    ));
                }
            }
        }
        let unresolved = Self::unresolved_enrichment_task_ids::<T>(plan, task_ids);
        if !unresolved.is_empty() {
            return Err(format!(
                "{} enrichment did not produce implementation_spec for task_ids={}",
                T::track_kind().as_str(),
                unresolved.join(",")
            ));
        }
        for task in plan.tasks.iter_mut() {
            let tid = task.task_id().to_string();
            if !task_ids.iter().any(|id| id == &tid) {
                continue;
            }
            T::apply_source_schema(task, source_schemas);
        }
        let enriched_count = task_ids.len() as u64;
        if enriched_count > 0 {
            let project_id = ctx.thread_id().clone().unwrap_or_default();
            let _ = crate::metering::global_metering()
                .record_batch(&[crate::metering::UsageEvent::PlanEnriched {
                    tasks: enriched_count,
                    project_id,
                }])
                .await;
        }
        Ok(())
    }

    pub(super) async fn enrich_cleanse_tasks(
        ctx: &AgentCtx,
        planning_context: &str,
        source_schemas: &crate::plan_types::SourceSchema,
        memo: &str,
        critique: &crate::plan_schema::PlanDesignCritiqueV1,
        plan: &mut crate::plan::CleansePlan,
        task_ids: &[String],
    ) -> Result<(), String> {
        Self::enrich_tasks::<crate::plan::CleanseTask>(
            ctx,
            planning_context,
            source_schemas,
            memo,
            critique,
            plan,
            task_ids,
        )
        .await
    }

    pub(super) async fn enrich_model_tasks(
        ctx: &AgentCtx,
        planning_context: &str,
        source_schemas: &crate::plan_types::SourceSchema,
        memo: &str,
        critique: &crate::plan_schema::PlanDesignCritiqueV1,
        plan: &mut crate::plan::ModelPlan,
        task_ids: &[String],
    ) -> Result<(), String> {
        Self::enrich_tasks::<crate::plan::ModelTask>(
            ctx,
            planning_context,
            source_schemas,
            memo,
            critique,
            plan,
            task_ids,
        )
        .await
    }

    pub(super) async fn generate_design_memo(
        ctx: &AgentCtx,
        track: TrackKind,
        planning_context: &str,
    ) -> Result<String, String> {
        use react_core::llm::{ChatMessage, ChatRole};
        let kind = if track.is_cleanse() {
            "cleanse_plan"
        } else {
            "model_plan"
        };
        let sys = prompts::plan::plan_design_memo_system_prompt(kind);
        let user = format!(
            "Planning kind: {kind}\n\nContext:\n{}\n\nWrite the design memo.",
            Self::excerpt(planning_context, 120_000)
        );
        let opts = Self::planning_llm_options(
            PlanningLlmProfile::DesignMemo,
            "data_engineer.plan_design_memo",
            ctx.thread_id().clone(),
        )?;
        ctx.llm_chat(
            &[
                ChatMessage {
                    role: ChatRole::System,
                    content: sys,
                },
                ChatMessage {
                    role: ChatRole::User,
                    content: user,
                },
            ],
            &opts,
        )
        .await
        .map_err(|e| e.to_string())
    }

    pub(super) async fn critique_design_memo(
        ctx: &AgentCtx,
        track: TrackKind,
        planning_context: &str,
        memo: &str,
        prior_critique: Option<&crate::plan_schema::PlanDesignCritiqueV1>,
    ) -> Result<crate::plan_schema::PlanDesignCritiqueV1, String> {
        use react_core::llm::{ChatMessage, ChatRole};
        let kind = if track.is_cleanse() {
            "cleanse_plan"
        } else {
            "model_plan"
        };
        let opts = Self::planning_llm_options(
            PlanningLlmProfile::DesignCritique,
            "data_engineer.plan_design_critique",
            ctx.thread_id().clone(),
        )?;
        let sys = prompts::plan::plan_design_critique_system_prompt(kind);
        let prior_section = match prior_critique {
            Some(prev) => format!(
                "\n\nPRIOR CRITIQUE (already addressed by revision):\n{}\n\
                 The memo was revised to address these. Only flag issues that REMAIN unaddressed.\n\
                 Do NOT introduce new concerns that were absent from the prior critique.",
                serde_json::to_string_pretty(prev).unwrap_or_else(|_| "{}".to_string()),
            ),
            None => String::new(),
        };
        let user = format!(
            "Planning kind: {kind}\n\nContext:\n{}\n\nDesign memo:\n{}{}\n\nReturn critique JSON.",
            Self::excerpt(planning_context, 80_000),
            Self::excerpt(memo, 40_000),
            prior_section,
        );
        let raw = ctx
            .llm_chat(
                &[
                    ChatMessage {
                        role: ChatRole::System,
                        content: sys,
                    },
                    ChatMessage {
                        role: ChatRole::User,
                        content: user,
                    },
                ],
                &opts,
            )
            .await
            .map_err(|e| e.to_string())?;
        let critique =
            Self::parse_json_typed_strict::<crate::plan_schema::PlanDesignCritiqueV1>(&raw)?;
        tracing::info!(
            target: "data_engineer",
            kind,
            ok = critique.ok,
            blockers = critique.blockers.len(),
            "critiqued plan design memo"
        );
        Ok(critique)
    }

    pub(super) async fn revise_design_memo(
        ctx: &AgentCtx,
        track: TrackKind,
        planning_context: &str,
        memo: &str,
        critique: &crate::plan_schema::PlanDesignCritiqueV1,
    ) -> Result<String, String> {
        use react_core::llm::{ChatMessage, ChatRole};
        let kind = if track.is_cleanse() {
            "cleanse_plan"
        } else {
            "model_plan"
        };
        let sys = prompts::plan::plan_design_memo_system_prompt(kind);
        let user = format!(
            "Planning kind: {kind}\n\nContext:\n{}\n\nCurrent design memo:\n{}\n\nCritique JSON:\n{}\n\nRewrite the design memo in free text so the critique blockers/fixes are addressed.\nDo not return JSON.",
            Self::excerpt(planning_context, 90_000),
            Self::excerpt(memo, 40_000),
            serde_json::to_string_pretty(critique).unwrap_or_else(|_| "{}".to_string()),
        );
        let opts = Self::planning_llm_options(
            PlanningLlmProfile::DesignMemo,
            "data_engineer.plan_design_memo_revise",
            ctx.thread_id().clone(),
        )?;
        ctx.llm_chat(
            &[
                ChatMessage {
                    role: ChatRole::System,
                    content: sys,
                },
                ChatMessage {
                    role: ChatRole::User,
                    content: user,
                },
            ],
            &opts,
        )
        .await
        .map_err(|e| e.to_string())
    }

    pub(super) async fn produce_critiqued_design_memo(
        ctx: &AgentCtx,
        track: TrackKind,
        planning_context: &str,
    ) -> Result<CritiquedDesignMemo, String> {
        let mut memo = Self::generate_design_memo(ctx, track, planning_context).await?;
        let critique =
            Self::critique_design_memo(ctx, track, planning_context, &memo, None).await?;
        if critique.ok {
            return Ok(CritiquedDesignMemo {
                memo,
                critique,
                disposition: DesignCritiqueDisposition::Accepted,
            });
        }

        memo = Self::revise_design_memo(ctx, track, planning_context, &memo, &critique).await?;
        let second_critique =
            Self::critique_design_memo(ctx, track, planning_context, &memo, Some(&critique))
                .await?;
        Ok(CritiquedDesignMemo {
            memo,
            disposition: resolve_design_critique_disposition(&critique, &second_critique),
            critique: second_critique,
        })
    }

    pub(super) fn critique_guidance(critique: &crate::plan_schema::PlanDesignCritiqueV1) -> String {
        if critique.blockers.is_empty() && critique.fixes.is_empty() {
            return "Design critique: no blockers identified.".to_string();
        }
        let blockers = if critique.blockers.is_empty() {
            "- (none)".to_string()
        } else {
            critique
                .blockers
                .iter()
                .take(6)
                .map(|b| {
                    let target = b
                        .target_id
                        .as_deref()
                        .map(|s| format!(" target={}", s))
                        .unwrap_or_default();
                    let detail = b
                        .detail
                        .as_deref()
                        .map(|s| format!(" detail={}", s.trim()))
                        .unwrap_or_default();
                    format!("- {:?}{}{}", b.code, target, detail)
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let fixes = if critique.fixes.is_empty() {
            "- (none)".to_string()
        } else {
            critique
                .fixes
                .iter()
                .take(6)
                .map(|f| {
                    let blocker = f
                        .blocker_code
                        .map(|c| format!(" blocker={:?}", c))
                        .unwrap_or_default();
                    let target = f
                        .target_id
                        .as_deref()
                        .map(|s| format!(" target={}", s))
                        .unwrap_or_default();
                    let detail = f
                        .detail
                        .as_deref()
                        .map(|s| format!(" detail={}", s.trim()))
                        .unwrap_or_default();
                    format!("- {:?}{}{}{}", f.action, blocker, target, detail)
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        format!(
            "Design critique (bounded one-pass):\n\
ok={}\n\
blockers:\n{}\n\
fixes:\n{}\n\
Apply these fixes in the output.",
            critique.ok, blockers, fixes
        )
    }

    pub(super) async fn generate_model_candidates(
        ctx: &AgentCtx,
        planning_context: &str,
        memo: &str,
        critique: &crate::plan_schema::PlanDesignCritiqueV1,
    ) -> Result<crate::plan_schema::ModelPlanCandidatesV1, String> {
        use react_core::llm::{ChatMessage, ChatRole};
        let mut opts = Self::planning_llm_options(
            PlanningLlmProfile::SkeletonOrCandidates,
            "data_engineer.model_plan_candidates",
            ctx.thread_id().clone(),
        )?;
        opts.expected_format = react_core::llm::LlmExpectedFormat::JsonSchema(
            react_core::schema_registry::OpenAiStrictSchema::for_type::<
                crate::plan_schema::ModelPlanCandidatesV1,
            >("suite.model_plan_candidates.v1")
            .map_err(|e| e.to_string())?,
        );
        let sys = prompts::plan::model_plan_candidates_system_prompt();
        let user = format!(
            "Context:\n{}\n\nDesign memo:\n{}\n\n{}\n\nReturn candidate-selection JSON.",
            Self::excerpt(planning_context, 60_000),
            Self::excerpt(memo, 30_000),
            Self::critique_guidance(critique)
        );
        let raw = ctx
            .llm_chat(
                &[
                    ChatMessage {
                        role: ChatRole::System,
                        content: sys.to_string(),
                    },
                    ChatMessage {
                        role: ChatRole::User,
                        content: user,
                    },
                ],
                &opts,
            )
            .await
            .map_err(|e| e.to_string())?;
        Self::parse_json_typed_strict::<crate::plan_schema::ModelPlanCandidatesV1>(&raw)
    }

    pub(super) fn compile_cleanse_skeleton_plan(
        skeleton: &crate::plan_schema::CleansePlanSkeletonV1,
    ) -> crate::plan::CleansePlan {
        let task_ids: Vec<String> = skeleton
            .tasks
            .iter()
            .map(|t| t.dataset_id.trim().to_string())
            .filter(|id| !id.is_empty())
            .collect();
        let tasks: Vec<crate::plan::CleanseTask> = task_ids
            .iter()
            .map(|dataset_id| crate::plan::CleanseTask {
                dataset_id: dataset_id.to_string(),
                expected_model_path: None,
                invariants: vec![],
                implementation_spec: None,
                source_schema: vec![],
                status: Default::default(),
                checklist: crate::plan::canonical_task_checklist(TrackKind::Cleanse),
            })
            .collect();
        let batches: Vec<Vec<String>> = if skeleton.batches.is_empty() {
            task_ids
                .chunks(plan_progress::MAX_BATCH_SIZE)
                .map(|c| c.to_vec())
                .collect()
        } else {
            skeleton.batches.clone()
        };
        let work_groups = crate::plan::canonical_work_groups_from_batches(&batches, "cleanse");
        crate::plan::CleansePlan {
            plan_key: String::new(),
            status: crate::plan::PlanStatus::Draft,
            project_snapshot: Default::default(),
            tasks,
            batches,
            work_groups,
            mutations: vec![],
            progress: Default::default(),
        }
    }

    pub(super) fn model_plan_min_score() -> i32 {
        env_util::model_plan_min_score()
    }

    pub(super) fn select_high_value_model_candidates(
        candidates: &[crate::plan_schema::ModelPlanCandidateV1],
    ) -> Vec<crate::plan_schema::ModelPlanCandidateV1> {
        if candidates.is_empty() {
            return vec![];
        }
        let min_score = Self::model_plan_min_score();
        let mut ranked = candidates.to_vec();
        ranked.sort_by(|a, b| {
            b.value_score
                .cmp(&a.value_score)
                .then_with(|| a.name.cmp(&b.name))
        });
        ranked
            .into_iter()
            .filter(|c| c.value_score >= min_score)
            .collect()
    }

    pub(super) fn compile_model_candidates_plan(
        candidates: &crate::plan_schema::ModelPlanCandidatesV1,
    ) -> crate::plan::ModelPlan {
        let selected = Self::select_high_value_model_candidates(&candidates.candidates);
        let task_names: Vec<String> = selected.into_iter().map(|c| c.name).collect();
        let batches: Vec<Vec<String>> = task_names.iter().map(|name| vec![name.clone()]).collect();
        let tasks: Vec<crate::plan::ModelTask> = task_names
            .into_iter()
            .map(|name| crate::plan::ModelTask {
                name,
                folder: crate::plan::ModelFolder::default(),
                goal: String::new(),
                inputs: vec![],
                expected_model_path: None,
                invariants: vec![],
                implementation_spec: None,
                source_schema: vec![],
                grounded_inputs: vec![],
                status: Default::default(),
                checklist: crate::plan::canonical_task_checklist(TrackKind::Model),
            })
            .collect();
        let work_groups =
            crate::plan::canonical_sequential_work_groups_from_batches(&batches, "model");
        crate::plan::ModelPlan {
            plan_key: String::new(),
            status: crate::plan::PlanStatus::Draft,
            project_snapshot: Default::default(),
            tasks,
            batches,
            work_groups,
            mutations: vec![],
            progress: Default::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn critique_disposition_rejects_repeated_blockers() {
        let prior = crate::plan_schema::PlanDesignCritiqueV1 {
            ok: false,
            blockers: vec![crate::plan_schema::PlanDesignBlockerV1 {
                code: crate::plan_schema::PlanDesignBlockerCodeV1::MissingTaskSpecs,
                target_id: None,
                detail: None,
            }],
            fixes: vec![],
        };
        let revised = crate::plan_schema::PlanDesignCritiqueV1 {
            ok: false,
            blockers: vec![crate::plan_schema::PlanDesignBlockerV1 {
                code: crate::plan_schema::PlanDesignBlockerCodeV1::MissingTaskSpecs,
                target_id: None,
                detail: None,
            }],
            fixes: vec![],
        };
        assert_eq!(
            resolve_design_critique_disposition(&prior, &revised),
            DesignCritiqueDisposition::Rejected
        );
    }

    #[test]
    fn critique_disposition_marks_novel_second_pass_blockers_explicitly() {
        let prior = crate::plan_schema::PlanDesignCritiqueV1 {
            ok: false,
            blockers: vec![crate::plan_schema::PlanDesignBlockerV1 {
                code: crate::plan_schema::PlanDesignBlockerCodeV1::MissingTaskSpecs,
                target_id: None,
                detail: None,
            }],
            fixes: vec![],
        };
        let revised = crate::plan_schema::PlanDesignCritiqueV1 {
            ok: false,
            blockers: vec![crate::plan_schema::PlanDesignBlockerV1 {
                code: crate::plan_schema::PlanDesignBlockerCodeV1::InvalidChecklistProgress,
                target_id: None,
                detail: None,
            }],
            fixes: vec![],
        };
        assert_eq!(
            resolve_design_critique_disposition(&prior, &revised),
            DesignCritiqueDisposition::AcceptedWithNovelBlockers
        );
    }

    #[test]
    fn generic_enrichment_applies_cleanse_spec_to_allowed_tasks() {
        let mut plan = crate::plan::CleansePlan {
            plan_key: "p".to_string(),
            status: crate::plan::PlanStatus::Draft,
            project_snapshot: Default::default(),
            tasks: vec![crate::plan::CleanseTask {
                dataset_id: "raw.orders".to_string(),
                expected_model_path: None,
                invariants: vec![],
                implementation_spec: None,
                source_schema: vec![],
                status: crate::plan::TaskStatus::Pending,
                checklist: vec![],
            }],
            batches: vec![vec!["raw.orders".to_string()]],
            work_groups: vec![],
            mutations: vec![],
            progress: Default::default(),
        };

        let (failed, errors) = DataEngineerSuite::apply_enrichment_items::<crate::plan::CleanseTask>(
            &mut plan,
            &["raw.orders".to_string()],
            vec![crate::plan_schema::CleansePlanEnrichmentItemV1 {
                task_id: "raw.orders".to_string(),
                implementation_spec: crate::plan::CleanseImplementationSpec {
                    spec_version: 1,
                    row_preserving: true,
                    output_fields: vec![crate::plan::OutputFieldSpec {
                        name: "order_id".to_string(),
                        kind: crate::plan::FieldKind::Raw,
                        source_columns: vec!["order_id".to_string()],
                        expression: "order_id".to_string(),
                        data_type: None,
                        nullable: false,
                        description: None,
                    }],
                    prohibited_ops: vec![],
                },
            }],
        );

        assert!(failed.is_empty(), "failed={failed:?} errors={errors:?}");
        assert!(errors.is_empty(), "errors={errors:?}");
        assert!(plan.tasks[0].implementation_spec.is_some());
    }

    #[test]
    fn generic_enrichment_applies_model_spec_and_updates_inputs_and_goal() {
        let mut plan = crate::plan::ModelPlan {
            plan_key: "p".to_string(),
            status: crate::plan::PlanStatus::Draft,
            project_snapshot: Default::default(),
            tasks: vec![crate::plan::ModelTask {
                name: "fct_orders".to_string(),
                folder: crate::plan::ModelFolder::Marts,
                goal: String::new(),
                inputs: vec![],
                expected_model_path: None,
                invariants: vec![],
                implementation_spec: None,
                source_schema: vec![],
                grounded_inputs: vec![],
                status: crate::plan::TaskStatus::Pending,
                checklist: vec![],
            }],
            batches: vec![vec!["fct_orders".to_string()]],
            work_groups: vec![],
            mutations: vec![],
            progress: Default::default(),
        };

        let (failed, errors) = DataEngineerSuite::apply_enrichment_items::<crate::plan::ModelTask>(
            &mut plan,
            &["fct_orders".to_string()],
            vec![crate::plan_schema::ModelPlanEnrichmentItemV1 {
                task_id: "fct_orders".to_string(),
                implementation_spec: crate::plan::ModelImplementationSpec {
                    spec_version: 1,
                    grain: "1 row per order_id".to_string(),
                    inputs: vec!["stg_orders".to_string()],
                    joins: vec![],
                    metrics: vec![],
                    output_fields: vec![],
                    assumptions: vec![],
                    evidence_claim_refs: vec![crate::providers::SemanticClaimRef {
                        claim_id: "candidate_key:test_raw.orders:order_id".to_string().into(),
                        kind: crate::providers::SemanticClaimKind::CandidateKey,
                        status: crate::providers::EvidenceStatus::Observed,
                    }],
                },
            }],
        );

        assert!(failed.is_empty(), "failed={failed:?} errors={errors:?}");
        assert!(errors.is_empty(), "errors={errors:?}");
        assert_eq!(plan.tasks[0].inputs, vec!["stg_orders".to_string()]);
        assert_eq!(
            plan.tasks[0].goal,
            "Build fct_orders from grounded staging inputs."
        );
        assert!(plan.tasks[0].implementation_spec.is_some());
    }
}
