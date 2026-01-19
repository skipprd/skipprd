use async_trait::async_trait;
use serde_json::Value;

use react_core::agent::AgentCtx;
use react_core::tools::Tool;

// This tool signals the application/CLI to obtain user input and append it as an observation.
// It always returns ok=true from the tool handler; the actual user response is captured outside
// of the agent loop and appended to the thread, after which the agent is resumed.
pub struct AskUserTool;

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &'static str {
        "ask_user"
    }

    async fn call(&self, args: Value, _ctx: &AgentCtx) -> Result<Value, String> {
        let prompt = args
            .get("prompt")
            .and_then(|x| x.as_str())
            .unwrap_or("Please provide additional context:");
        Ok(serde_json::json!({"ok": true, "prompt": prompt}))
    }
}

