use serde::{Deserialize, Serialize};

/// Standardized suite output type (moved from legacy `flows/adapter.rs`).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum FlowFrame {
    /// Terminal response for an agent run.
    Final {
        answer: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        sql: Option<String>,
    },
    /// Agent needs user input.
    AwaitUser { prompt: String },
    /// Agent requires explicit approval (artifact diffs, etc).
    AwaitApproval { prompt: String },
}

