// Declared as `mod review_prompts;` in data_engineer/mod.rs

use schemars::JsonSchema;
use serde_json::Value;

use super::plan_kind::PlanKind;
use super::review_batched::ProjectFile;

fn render_output_schema<T: JsonSchema>() -> String {
    crate::plan_schema::strict_schema_for::<T>()
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| "{\"type\":\"object\"}".to_string())
}

pub(super) fn system_prompt_for_summary(plan_kind: Option<PlanKind>) -> String {
    let tier_focus = if plan_kind == Some(PlanKind::Cleanse) {
        "- Tier focus (CRITICAL): this is a SILVER (cleanse) review. Do NOT penalize missing GOLD models.\n"
    } else {
        ""
    };
    format!(
        "You are a read-only reviewer for a DBT analytics project.\n\n\
         You will be given:\n\
         - The original goal and review context (brief)\n\
         - A small, deterministic project snapshot (dbt_project.yml, sources list, model file index, and minimal manifest metadata)\n\n\
         Output one JSON object matching this JSON schema:\n\
         {schema}\n\n\
         Rules:\n\
         - FACTUAL ONLY: project_notes must describe what IS present (structure, naming, sources, layers) — not what MIGHT be wrong.\n\
         - Do NOT speculate about risks, missing features, or improvements. That is the batch reviewer's job (it sees the actual SQL/YAML).\n\
         - Keep notes concise and high-signal.\n\
         {tier_focus}",
        schema = render_output_schema::<crate::domain_types::ReviewSummaryOutput>(),
    )
}

pub(super) fn system_prompt_for_batch(plan_kind: Option<PlanKind>) -> String {
    let (tier_focus, quality_lens) = if plan_kind == Some(PlanKind::Cleanse) {
        (
            "- Tier focus (CRITICAL): this is a SILVER (cleanse) review. Do NOT critique missing GOLD models.\n",
            "- Quality lens (SILVER): assess whether the cleanse layer is a stable, row-preserving foundation — explicit fields, safe casting, quality flags, stable naming.\n",
        )
    } else {
        (
            "",
            "- Quality lens (GOLD): assess whether the model layer enables meaningful business decisions — not just technically-correct SQL. Flag missing business semantics (definitions, time axis, entity meaning, join contracts) only when visible in the provided SQL.\n",
        )
    };
    format!(
        r#"You are a read-only reviewer for a DBT analytics project.

You will be given:
- The original goal and review context
- A batch of items (datasets or model names)
- The expected model file paths and their contents (bounded)
- Any invariants/notes from planning
- The authoritative schema (columns/types) for each dataset in the batch (when available)
- Typed semantic evidence references from the implementation_spec (when available)

Output one JSON object matching this JSON schema:
{schema}

Rules:
- CRITICAL: The planning artifacts you receive (invariants/notes and any implementation_spec) are the authoritative design contract for this phase.
  - Your primary job is CONFORMANCE REVIEW: does the SQL/YAML implement the provided implementation_spec and obey prohibited_ops?
  - For GOLD/model review, also check evidence conformance: grain, key tests, relationships, casts used by metrics, and aggregate safety must be backed by observed or user_provided evidence_claim_refs.
  - If SQL/YAML relies on unverified/contradicted semantic evidence, prefix the finding with "REQUIRES PLAN CHANGE: ...".
  - Do NOT propose changing the contract as part of review. If you believe the contract itself is wrong/ambiguous, prefix the finding with "REQUIRES PLAN CHANGE: ..." and do NOT propose an implementation change.
- EVIDENCE-ONLY: every finding in "findings" MUST cite a specific file path, column name, or SQL construct you can see in the provided batch contents. Do NOT speculate about files or code you have not been shown.
- MATERIALITY THRESHOLD: only flag issues that would cause incorrect query results, broken compilation, or violate an explicit prohibition in the spec. Do NOT flag stylistic preferences, naming conventions, or "nice to have" improvements unless they violate the spec.
- If no concrete, evidence-backed findings exist for this batch, return {{"findings": []}}.
- Hard cap: at most 3 findings total. Prefer fewer; empty findings is the ideal outcome for conformant code.
- Each finding must be decision-oriented: impacted metric/decision, concrete evidence from the provided SQL/schema, and smallest next action to reduce risk.
- IMPORTANT: Do NOT suggest adding/selecting fields that are not present in the provided authoritative schema.
{tier_focus}{quality_lens}- No tool calls and no file edits."#,
        schema = render_output_schema::<crate::domain_types::ReviewBatchOutput>(),
    )
}

pub(super) fn system_prompt_for_unify(plan_kind: Option<PlanKind>) -> String {
    let (tier_rule, target_rule) = if plan_kind == Some(PlanKind::Cleanse) {
        (
            "Tier rules: this is a SILVER (cleanse) review. Set tier=\"silver\".",
            "Target rules: target_task_ids means affected plan task IDs exactly as shown in batch_items.",
        )
    } else {
        (
            "Tier rules: this is a GOLD/model review. Set tier=\"gold\".",
            "Target rules: target_task_ids means affected plan task IDs exactly as shown in batch_items. Do not use file paths, warehouse relation FQNs, or source catalog dataset IDs.",
        )
    };
    format!(
        "You are a read-only reviewer for a DBT analytics project.\n\n\
         You will be given:\n\
         - The original goal and review context\n\
         - Project-level notes (factual context only)\n\
         - Findings from ALL review batches (evidence-based)\n\n\
         You must output one JSON object matching this JSON schema:\n\
         {schema}\n\n\
         Interpretation rules (CRITICAL):\n\
         - decision=\"proceed\" means: no action required now; the implementation conforms and there are no net-new/still-unresolved high-value issues.\n\
         - decision=\"patch_impl\" means: a concrete, high-severity implementation defect exists that MUST be fixed NOW to avoid incorrect query results or broken compilation — NOT stylistic or speculative improvements.\n\
         - decision=\"plan_change\" means: the implementation fundamentally contradicts the approved plan/spec in a way that CANNOT be resolved by patching the implementation alone. This is rare — use only for genuine contradictions, not debatable design preferences.\n\n\
         Decision bias (CRITICAL):\n\
         - DEFAULT TO \"proceed\" unless there is a clear, high-severity conformance violation. The implementation has already passed dbt validation (compilation and tests). Stylistic improvements, naming suggestions, and \"nice to have\" enhancements are NOT grounds for patch_impl or plan_change.\n\
         - If the review context mentions a prior review cycle, apply a HIGHER BAR for patch_impl: only block on issues that are strictly worse than what was already reviewed. Findings that were visible in the prior cycle but not flagged should be considered implicitly accepted.\n\n\
         {tier_rule}\n\
         {target_rule}\n\n\
         Unify requirements (CRITICAL):\n\
         - EVIDENCE-ONLY: you may ONLY promote issues that appear in the batch findings. Do NOT introduce new issues based on project_notes or general knowledge.\n\
         - If the batch findings are empty, set decision=\"proceed\" and keep the body brief.\n\
         - If the main finding is prefixed \"REQUIRES PLAN CHANGE\", set decision=\"plan_change\".\n\
         - For decision=\"plan_change\", populate target_task_ids with the specific affected plan task IDs pertaining to the plan change suggestions. Leave target_task_ids empty only when the approved plan truly needs a complete rewrite.\n\
         - Set decision=\"patch_impl\" only when a batch finding identifies a concrete defect that would produce incorrect data or broken queries — not for improvements or style.\n\
         - Hard cap: at most 3 issues total across the final review body.\n\
         - Delta-first: suppress repeated advice that has no new evidence since the prior review context. If a finding was present in the prior review and the implementation was already patched for it, do NOT re-flag unless the patch introduced a new defect.",
        schema = render_output_schema::<crate::domain_types::ReviewUnifyOutput>(),
        target_rule = target_rule,
    )
}

pub(super) fn compact_review_context_for_summary(question_with_context: &str) -> String {
    let mut entry_reason_code: Option<String> = None;
    let mut entry_reason_detail_raw: Option<String> = None;

    for line in question_with_context.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("- entry_reason_code:") {
            let v = rest.trim();
            if !v.is_empty() {
                entry_reason_code = Some(v.to_string());
            }
        }
        if let Some(rest) = t.strip_prefix("- entry_reason_detail:") {
            let v = rest.trim();
            if !v.is_empty() {
                entry_reason_detail_raw = Some(v.to_string());
            }
        }
    }

    let mut out = String::new();
    if let Some(rc) = entry_reason_code.as_deref() {
        out.push_str("entry_reason_code: ");
        out.push_str(rc);
        out.push('\n');
    }

    if let Some(raw) = entry_reason_detail_raw.as_deref() {
        let hash = react_core::llm_observability::sha256_hex_str(raw);
        out.push_str("entry_reason_detail_sha256: ");
        out.push_str(&hash);
        out.push('\n');

        if let Ok(v) = serde_json::from_str::<Value>(raw) {
            if let Some(idx) = v.get("batch_idx").and_then(|x| x.as_u64()) {
                out.push_str(&format!("entry_reason_detail.batch_idx: {}\n", idx));
            }
            if let Some(items) = v.get("batch_items").and_then(|x| x.as_array()) {
                let mut names: Vec<String> = items
                    .iter()
                    .filter_map(|it| it.as_str().map(|s| s.to_string()))
                    .collect();
                names.sort();
                names.dedup();
                let keep = names.len().min(5);
                out.push_str(&format!(
                    "entry_reason_detail.batch_items_head (sorted, {} of {}): {}\n",
                    keep,
                    names.len(),
                    names.into_iter().take(keep).collect::<Vec<_>>().join(", ")
                ));
            }
            if let Some(keys) = v.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()) {
                let mut kk = keys;
                kk.sort();
                out.push_str(&format!(
                    "entry_reason_detail.keys_sorted: {}\n",
                    kk.into_iter().take(20).collect::<Vec<_>>().join(", ")
                ));
            }
        }
    }

    if out.trim().is_empty() {
        question_with_context.to_string()
    } else {
        out
    }
}

pub(super) fn render_path_list(label: &str, paths: &[String], max_items: usize) -> String {
    let mut p = paths.to_vec();
    p.sort();
    p.dedup();
    let total = p.len();
    let keep = total.min(max_items);
    let shown = &p[..keep];
    let mut s = String::new();
    s.push_str(label);
    s.push_str(&format!(" (total={}):\n", total));
    for it in shown {
        s.push_str("- ");
        s.push_str(it);
        s.push('\n');
    }
    if total > keep {
        s.push_str(&format!(
            "... omitted {} more (deterministic head)\n",
            total - keep
        ));
    }
    s
}

pub(super) fn render_files(files: &[ProjectFile]) -> String {
    let mut s = String::new();
    for f in files {
        s.push_str("\n---\npath: ");
        s.push_str(&f.path);
        s.push_str("\n\n");
        s.push_str(&f.content);
        s.push_str("\n");
    }
    s
}
