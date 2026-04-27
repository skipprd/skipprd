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

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PlanContextPacket {
    #[serde(default)]
    pub plan_kind: Option<PlanKind>,
    #[serde(default)]
    pub plan_key: Option<String>,
    /// Rendered, human-readable context (bounded by the builder).
    #[serde(default)]
    pub context_text: Option<String>,
    /// Optional stable identifiers for delta retries.
    #[serde(default)]
    pub unresolved_ids: Vec<String>,
    #[serde(default)]
    pub new_evidence_refs: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AuthorBatchPacket {
    /// Stable ids/names for the next deterministic batch items.
    #[serde(default)]
    pub batch_items: Vec<String>,
    /// Optional rendered details (bounded).
    #[serde(default)]
    pub details_text: Option<String>,
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
    pub batch: Option<AuthorBatchPacket>,
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
}
