use datafusion::prelude::SessionContext;
use crate::qa::tools::ToolRegistry;

pub fn build_for(agent: &str, ctx: &SessionContext) -> ToolRegistry {
	// Reuse existing builder to stay DRY
	crate::ws::agent_runner::build_registry(agent, ctx)
}


