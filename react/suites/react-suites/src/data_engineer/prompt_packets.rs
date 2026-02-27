use serde::{Deserialize, Serialize};

use crate::data_engineer::plan_kind::PlanKind;
use crate::data_engineer::progress_controller::RepairLadderStep;

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

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RepairPacket {
    pub target_path: String,
    #[serde(default)]
    pub ladder_step: RepairLadderStep,
    #[serde(default)]
    pub last_validate_brief: Option<String>,
    /// Tool contract text (canonical, not generated).
    #[serde(default)]
    pub patch_contract: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PromptEnvelope {
    pub phase: String,
    pub goal: String,
    #[serde(default)]
    pub plan: Option<PlanContextPacket>,
    #[serde(default)]
    pub batch: Option<AuthorBatchPacket>,
    #[serde(default)]
    pub repair: Option<RepairPacket>,
}

pub fn validate_envelope(envelope: &PromptEnvelope) -> Result<(), String> {
    if envelope.phase.trim().is_empty() {
        return Err("prompt envelope phase is required".to_string());
    }
    if envelope.goal.trim().is_empty() {
        return Err("prompt envelope goal is required".to_string());
    }
    if envelope.plan.is_none() && envelope.batch.is_none() && envelope.repair.is_none() {
        return Err("prompt envelope requires at least one packet (plan|batch|repair)".to_string());
    }
    if let Some(batch) = envelope.batch.as_ref() {
        if batch.batch_items.is_empty() {
            return Err("prompt envelope batch packet requires non-empty batch_items".to_string());
        }
    }
    if let Some(repair) = envelope.repair.as_ref() {
        if envelope.plan.is_some() || envelope.batch.is_some() {
            return Err(
                "repair prompt envelope must not include plan or batch packets in repair mode"
                    .to_string(),
            );
        }
        if repair.target_path.trim().is_empty() {
            return Err("repair prompt envelope requires target_path".to_string());
        }
        let missing_contract = repair
            .patch_contract
            .as_deref()
            .map(|s| s.trim().is_empty())
            .unwrap_or(true);
        if missing_contract {
            return Err("repair prompt envelope requires non-empty patch_contract".to_string());
        }
    }
    Ok(())
}

pub fn render_envelope(envelope: &PromptEnvelope) -> Result<String, String> {
    validate_envelope(envelope)?;
    let mut s = String::new();
    s.push_str(&format!("Goal: {}\n", envelope.goal.trim()));
    s.push_str(&format!("Phase: {}\n\n", envelope.phase.trim()));
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
            phase: "model_author".to_string(),
            goal: "apply the next deterministic step".to_string(),
            ..PromptEnvelope::default()
        };
        assert!(render_envelope(&envelope).is_err());
    }

    #[test]
    fn render_envelope_rejects_invalid_repair_shape() {
        let envelope = PromptEnvelope {
            phase: "cleanse_author".to_string(),
            goal: "repair the failing model".to_string(),
            plan: Some(PlanContextPacket::default()),
            repair: Some(RepairPacket {
                target_path: "".to_string(),
                ..RepairPacket::default()
            }),
            ..PromptEnvelope::default()
        };
        assert!(render_envelope(&envelope).is_err());
    }
}
