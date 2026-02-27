pub use react_core::resolved_config::*;

use react_core::agent::AgentCtx;

/// Extract the resolved config from an `AgentCtx`.
pub fn resolved_config_from_ctx(ctx: &AgentCtx) -> Option<&ReactResolvedConfig> {
    ctx.resolved_config.as_ref().map(|c| c.as_ref())
}
