//! Prompt envelope, context packet, and batch packet types — plus a typestate
//! builder that makes it impossible to set the same context field twice.
//!
//! ## Why a typestate?
//!
//! The previous design exposed `PlanContextPacket.context_text: Option<String>`
//! into which callers concatenated planning_context, design_memo,
//! critique_guidance, and plan_summary. Nothing in the type system prevented
//! the same content from being set twice or smuggled in via two different
//! fields, which is exactly how duplicate-envelope bugs creep in.
//!
//! Each of the four required context slots is now a distinct field on
//! [`PlanContextPacket`], and [`PromptContextBuilder`] enforces "set at most
//! once" at compile time using zero-sized `Unset`/`Set` markers. Setting the
//! same field twice is a compile error (no setter is in scope after the slot
//! transitions to `Set`). Calling `build()` without all four required slots
//! filled is also a compile error.

use std::marker::PhantomData;

use serde::{Deserialize, Serialize};

use crate::control_flow::Phase;
use crate::plan_kind::PlanKind;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TurnDirective {
    Reason,
    Compile,
    Verify,
    Advance,
}

impl Default for TurnDirective {
    fn default() -> Self {
        Self::Reason
    }
}

/// Structured plan-context packet.
///
/// Each of `planning_context`, `design_memo`, `critique_guidance`, and
/// `plan_summary` is its own field — they MUST NOT be concatenated into one
/// freeform blob upstream. Construct via [`PromptContextBuilder`] for the
/// compile-time at-most-once-per-field guarantee.
#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PlanContextPacket {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_kind: Option<PlanKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planning_context: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_memo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub critique_guidance: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_summary: Option<String>,
    /// Reason-phase memo carried into the Compile prompt. Subsumes the
    /// previous freeform `compile_prompt_from_reason` concatenation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_memo: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unresolved_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub new_evidence_refs: Vec<String>,
}

/// Typed batch packet covering the trailers that used to be freeform
/// `format!(...)` concatenations after the envelope (target task_ids list,
/// per-task schema block, retry hint).
#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EnrichmentBatchPacket {
    /// Stable ids/names for the next deterministic batch items.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub batch_items: Vec<String>,
    /// Per-task source-column schema, rendered adjacent to the task ids.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_task_schema: Option<String>,
    /// Strict retry guidance emitted only on the Verify directive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_hint: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PromptEnvelope {
    pub phase: Phase,
    pub goal: String,
    pub directive: TurnDirective,
    #[serde(default)]
    pub plan: Option<PlanContextPacket>,
    #[serde(default)]
    pub batch: Option<EnrichmentBatchPacket>,
}

pub fn validate_envelope(envelope: &PromptEnvelope) -> Result<(), String> {
    if envelope.goal.trim().is_empty() {
        return Err("prompt envelope goal is required".to_string());
    }
    if envelope.plan.is_none() && envelope.batch.is_none() {
        return Err("prompt envelope requires at least one packet (plan|batch)".to_string());
    }
    if let Some(batch) = envelope.batch.as_ref() {
        if batch.batch_items.is_empty() {
            return Err("prompt envelope batch packet requires non-empty batch_items".to_string());
        }
    }
    Ok(())
}

pub fn render_envelope(envelope: &PromptEnvelope) -> Result<String, String> {
    validate_envelope(envelope)?;
    let mut s = String::new();
    s.push_str(&format!("Goal: {}\n", envelope.goal.trim()));
    s.push_str(&format!("Phase: {}\n\n", envelope.phase.as_str()));
    s.push_str("Context packet (typed envelope):\n");
    s.push_str(&serde_json::to_string_pretty(envelope).unwrap_or_else(|_| "{}".to_string()));
    s.push('\n');
    Ok(s)
}

// ─────────────────────────────────────────────────────────────────────────────
// Typestate builder
// ─────────────────────────────────────────────────────────────────────────────

/// Marker: the corresponding required slot has not yet been provided.
pub struct Unset;
/// Marker: the corresponding required slot has been provided.
pub struct Set;

/// Compile-time at-most-once builder for [`PlanContextPacket`].
///
/// Type parameters `C, M, Cr, S` track whether `planning_context`,
/// `design_memo`, `critique_guidance`, and `plan_summary` (respectively)
/// have been set. The setter for each required field is only in scope while
/// its type parameter is [`Unset`]; calling it transitions that parameter to
/// [`Set`]. `build()` is only implemented for the all-`Set` state.
///
/// ### Compile-fail examples
///
/// Setting the same required field twice is rejected at compile time:
///
/// ```compile_fail
/// # use react_suite_data_engineer::prompt_packets::PromptContextBuilder;
/// let _ = PromptContextBuilder::new()
///     .planning_context("a")
///     .planning_context("b"); // ERROR: planning_context already Set
/// ```
///
/// Building before all four required fields are provided is also rejected:
///
/// ```compile_fail
/// # use react_suite_data_engineer::prompt_packets::PromptContextBuilder;
/// let _ = PromptContextBuilder::new()
///     .planning_context("a")
///     .design_memo("b")
///     .build(); // ERROR: critique_guidance, plan_summary still Unset
/// ```
pub struct PromptContextBuilder<C, M, Cr, S> {
    plan_kind: Option<PlanKind>,
    plan_key: Option<String>,
    planning_context: Option<String>,
    design_memo: Option<String>,
    critique_guidance: Option<String>,
    plan_summary: Option<String>,
    reasoning_memo: Option<String>,
    unresolved_ids: Vec<String>,
    new_evidence_refs: Vec<String>,
    _markers: PhantomData<fn() -> (C, M, Cr, S)>,
}

impl Default for PromptContextBuilder<Unset, Unset, Unset, Unset> {
    fn default() -> Self {
        Self::new()
    }
}

impl PromptContextBuilder<Unset, Unset, Unset, Unset> {
    pub fn new() -> Self {
        Self {
            plan_kind: None,
            plan_key: None,
            planning_context: None,
            design_memo: None,
            critique_guidance: None,
            plan_summary: None,
            reasoning_memo: None,
            unresolved_ids: Vec::new(),
            new_evidence_refs: Vec::new(),
            _markers: PhantomData,
        }
    }
}

// Required-slot setters: each consumes the builder and flips exactly one
// marker from `Unset` to `Set`. The method is unreachable once the slot is
// already `Set`, which is what makes "set twice" a compile error.

impl<M, Cr, S> PromptContextBuilder<Unset, M, Cr, S> {
    pub fn planning_context(
        self,
        v: impl Into<String>,
    ) -> PromptContextBuilder<Set, M, Cr, S> {
        PromptContextBuilder {
            plan_kind: self.plan_kind,
            plan_key: self.plan_key,
            planning_context: Some(v.into()),
            design_memo: self.design_memo,
            critique_guidance: self.critique_guidance,
            plan_summary: self.plan_summary,
            reasoning_memo: self.reasoning_memo,
            unresolved_ids: self.unresolved_ids,
            new_evidence_refs: self.new_evidence_refs,
            _markers: PhantomData,
        }
    }
}

impl<C, Cr, S> PromptContextBuilder<C, Unset, Cr, S> {
    pub fn design_memo(self, v: impl Into<String>) -> PromptContextBuilder<C, Set, Cr, S> {
        PromptContextBuilder {
            plan_kind: self.plan_kind,
            plan_key: self.plan_key,
            planning_context: self.planning_context,
            design_memo: Some(v.into()),
            critique_guidance: self.critique_guidance,
            plan_summary: self.plan_summary,
            reasoning_memo: self.reasoning_memo,
            unresolved_ids: self.unresolved_ids,
            new_evidence_refs: self.new_evidence_refs,
            _markers: PhantomData,
        }
    }
}

impl<C, M, S> PromptContextBuilder<C, M, Unset, S> {
    pub fn critique_guidance(
        self,
        v: impl Into<String>,
    ) -> PromptContextBuilder<C, M, Set, S> {
        PromptContextBuilder {
            plan_kind: self.plan_kind,
            plan_key: self.plan_key,
            planning_context: self.planning_context,
            design_memo: self.design_memo,
            critique_guidance: Some(v.into()),
            plan_summary: self.plan_summary,
            reasoning_memo: self.reasoning_memo,
            unresolved_ids: self.unresolved_ids,
            new_evidence_refs: self.new_evidence_refs,
            _markers: PhantomData,
        }
    }
}

impl<C, M, Cr> PromptContextBuilder<C, M, Cr, Unset> {
    pub fn plan_summary(self, v: impl Into<String>) -> PromptContextBuilder<C, M, Cr, Set> {
        PromptContextBuilder {
            plan_kind: self.plan_kind,
            plan_key: self.plan_key,
            planning_context: self.planning_context,
            design_memo: self.design_memo,
            critique_guidance: self.critique_guidance,
            plan_summary: Some(v.into()),
            reasoning_memo: self.reasoning_memo,
            unresolved_ids: self.unresolved_ids,
            new_evidence_refs: self.new_evidence_refs,
            _markers: PhantomData,
        }
    }
}

// Optional fields: available in every state (no typestate transition).

impl<C, M, Cr, S> PromptContextBuilder<C, M, Cr, S> {
    pub fn plan_kind(mut self, v: PlanKind) -> Self {
        self.plan_kind = Some(v);
        self
    }

    pub fn plan_key(mut self, v: impl Into<String>) -> Self {
        self.plan_key = Some(v.into());
        self
    }

    pub fn unresolved_ids(mut self, v: Vec<String>) -> Self {
        self.unresolved_ids = v;
        self
    }

    pub fn new_evidence_refs(mut self, v: Vec<String>) -> Self {
        self.new_evidence_refs = v;
        self
    }
}

impl PromptContextBuilder<Set, Set, Set, Set> {
    /// Finalize into a [`PlanContextPacket`]. Available only when all four
    /// required slots have been provided exactly once.
    pub fn build(self) -> PlanContextPacket {
        PlanContextPacket {
            plan_kind: self.plan_kind,
            plan_key: self.plan_key,
            planning_context: self.planning_context,
            design_memo: self.design_memo,
            critique_guidance: self.critique_guidance,
            plan_summary: self.plan_summary,
            reasoning_memo: self.reasoning_memo,
            unresolved_ids: self.unresolved_ids,
            new_evidence_refs: self.new_evidence_refs,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Enrichment envelope helper
// ─────────────────────────────────────────────────────────────────────────────

/// Convenience builder that wraps a [`PlanContextPacket`] + optional batch
/// into a fully rendered envelope string for an enrichment LLM call.
///
/// The required context is provided as a finalized [`PlanContextPacket`]
/// (built by [`PromptContextBuilder::build`]), so this builder is only
/// constructible after the typestate guarantee has held. The same packet can
/// be reused across `Reason` and `Compile` calls for the same chunk; only the
/// directive (and optional `reasoning_memo` / `batch`) differ.
pub struct EnrichmentEnvelopeBuilder {
    phase: Phase,
    directive: TurnDirective,
    plan: PlanContextPacket,
    batch: Option<EnrichmentBatchPacket>,
}

impl EnrichmentEnvelopeBuilder {
    pub fn new(phase: Phase, directive: TurnDirective, plan: PlanContextPacket) -> Self {
        Self {
            phase,
            directive,
            plan,
            batch: None,
        }
    }

    pub fn batch(mut self, batch: EnrichmentBatchPacket) -> Self {
        self.batch = Some(batch);
        self
    }

    /// Render the envelope to a final LLM-ready string with directive-specific
    /// prologue/epilogue. This is the single place where enrichment turn
    /// directives map to their natural-language framing — there are no
    /// freeform `format!` trailers downstream.
    pub fn build(self) -> String {
        let directive = self.directive.clone();
        let envelope = PromptEnvelope {
            phase: self.phase,
            goal: "Produce implementation_spec content for target tasks".to_string(),
            directive: self.directive,
            plan: Some(self.plan),
            batch: self.batch,
        };
        let body = render_envelope(&envelope).unwrap_or_else(|_| "{}".to_string());
        match directive {
            TurnDirective::Reason => format!(
                "Think through the enrichment strategy for these task_ids. Return plain text only, no JSON.\n\n{body}"
            ),
            TurnDirective::Compile | TurnDirective::Verify => {
                format!("{body}\nReturn schema-valid enrichment JSON.")
            }
            TurnDirective::Advance => body,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_envelope_rejects_missing_packets() {
        let envelope = PromptEnvelope {
            phase: Phase::ModelAuthor,
            goal: "apply the next deterministic step".to_string(),
            directive: TurnDirective::Advance,
            plan: None,
            batch: None,
        };
        assert!(render_envelope(&envelope).is_err());
    }

    #[test]
    fn typestate_builder_renders_all_required_fields_separately() {
        let packet = PromptContextBuilder::new()
            .planning_context("CTX")
            .design_memo("MEMO")
            .critique_guidance("CRIT")
            .plan_summary("SUM")
            .build();
        let json = serde_json::to_string(&packet).expect("serialize");
        assert!(json.contains("\"planning_context\":\"CTX\""));
        assert!(json.contains("\"design_memo\":\"MEMO\""));
        assert!(json.contains("\"critique_guidance\":\"CRIT\""));
        assert!(json.contains("\"plan_summary\":\"SUM\""));
    }

    #[test]
    fn reasoning_memo_renders_in_compile_envelope_not_reason() {
        // The reason envelope is built without setting reasoning_memo.
        let reason_packet = PromptContextBuilder::new()
            .planning_context("CTX")
            .design_memo("MEMO")
            .critique_guidance("CRIT")
            .plan_summary("SUM")
            .build();
        let reason_rendered =
            EnrichmentEnvelopeBuilder::new(Phase::CleansePlan, TurnDirective::Reason, reason_packet)
                .build();
        assert!(
            !reason_rendered.contains("reasoning_memo"),
            "reason envelope must NOT carry reasoning_memo; got: {reason_rendered}"
        );

        // Compile envelope sets reasoning_memo — it must appear in the rendered JSON.
        let mut compile_packet = PromptContextBuilder::new()
            .planning_context("CTX")
            .design_memo("MEMO")
            .critique_guidance("CRIT")
            .plan_summary("SUM")
            .build();
        compile_packet.reasoning_memo = Some("REASON-MEMO".to_string());
        let compile_rendered = EnrichmentEnvelopeBuilder::new(
            Phase::CleansePlan,
            TurnDirective::Compile,
            compile_packet,
        )
        .build();
        assert!(
            compile_rendered.contains("REASON-MEMO"),
            "compile envelope must carry reasoning_memo; got: {compile_rendered}"
        );
    }

    #[test]
    fn batch_packet_carries_per_task_schema_and_retry_hint() {
        let packet = PromptContextBuilder::new()
            .planning_context("CTX")
            .design_memo("MEMO")
            .critique_guidance("CRIT")
            .plan_summary("SUM")
            .build();
        let batch = EnrichmentBatchPacket {
            batch_items: vec!["t1".to_string(), "t2".to_string()],
            per_task_schema: Some("AVAILABLE COLUMNS FOR t1...".to_string()),
            retry_hint: Some("STRICT RETRY REQUIREMENTS...".to_string()),
        };
        let rendered =
            EnrichmentEnvelopeBuilder::new(Phase::CleansePlan, TurnDirective::Verify, packet)
                .batch(batch)
                .build();
        assert!(rendered.contains("AVAILABLE COLUMNS FOR t1"));
        assert!(rendered.contains("STRICT RETRY REQUIREMENTS"));
        assert!(rendered.contains("\"t1\""));
        assert!(rendered.contains("\"t2\""));
    }
}
