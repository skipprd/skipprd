use async_trait::async_trait;
use serde_json::Value;

use react_core::agent::AgentCtx;
use react_core::tools::Tool;

// This tool is used for explicit approval flows (e.g., DBT artifact approvals).
// It mirrors ask_user's behavior but is semantically distinct so UIs can render Approve/Reject.
pub struct AskApprovalTool;

#[async_trait]
impl Tool for AskApprovalTool {
    fn name(&self) -> &'static str {
        "ask_approval"
    }

    async fn call(&self, args: Value, _ctx: &AgentCtx) -> Result<Value, String> {
        let prompt = args
            .get("prompt")
            .and_then(|x| x.as_str())
            .unwrap_or("Please review and approve/reject:")
            .to_string();
        Ok(serde_json::json!({"ok": true, "prompt": prompt}))
    }
}
