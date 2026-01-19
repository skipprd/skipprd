use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    /// Non-terminal reviewer output (read-only).
    Review {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        meta: Option<Value>,
    },
    /// Agent needs user input.
    AwaitUser { prompt: String },
    /// Agent requires explicit approval (artifact diffs, etc).
    AwaitApproval { prompt: String },
}

