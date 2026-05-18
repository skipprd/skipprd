use async_trait::async_trait;
use serde_json::Value;

use react_core::agent::AgentCtx;
use react_core::tools::Tool;

pub struct ModelSubagentTool;

#[async_trait]
impl Tool for ModelSubagentTool {
    fn name(&self) -> &'static str {
        "model_subagent"
    }

    async fn call(&self, args: Value, _ctx: &AgentCtx) -> Result<Value, String> {
        if super::skippr_cli::ide_chat_surface_enabled() {
            return super::skippr_cli::ide_model_bridge_request(&args);
        }
        let cli_args = super::skippr_cli::build_model_args(&args)?;
        super::skippr_cli::run_skippr_model(cli_args)
    }
}
