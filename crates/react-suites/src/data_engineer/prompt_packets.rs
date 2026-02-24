use serde::{Deserialize, Serialize};

use crate::data_engineer::repair_state::RepairLadderStep;

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PlanContextPacket {
    #[serde(default)]
    pub plan_kind: Option<String>,
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

pub fn render_envelope(envelope: &PromptEnvelope) -> String {
    let mut s = String::new();
    s.push_str(&format!("Goal: {}\n", envelope.goal.trim()));
    s.push_str(&format!("Phase: {}\n\n", envelope.phase.trim()));
    s.push_str("Context packet (typed envelope):\n");
    s.push_str(&serde_json::to_string_pretty(envelope).unwrap_or_else(|_| "{}".to_string()));
    s.push('\n');
    s
}
