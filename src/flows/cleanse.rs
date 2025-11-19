use datafusion::prelude::SessionContext;
use crate::flows::adapter::FlowFrame;

pub async fn run(thread_id: &str, question: &str) -> Result<Vec<FlowFrame>, String> {
	let sys = crate::prompts::prompts_shared::with_time_context(crate::qa::prompts::system_prompt());
	let tools_card = crate::qa::prompts::tool_card();

	let ctx_df = SessionContext::new();
	crate::ws::agent_runner::pre_register_all_namespaces(&ctx_df).await;
	let registry = crate::flows::registry::build_for("cleanse", &ctx_df);
	let actx = crate::qa::agent::AgentCtx {
		top_k: 30,
		per_step_timeout_secs: 10,
		max_steps: 10,
		thread_id: Some(thread_id.to_string()),
		progress_tx: None,
		pre_step_tx: None,
		agent_name: Some("cleanse".to_string()),
		dataset_candidates: Vec::new(),
	};
	let mut frames: Vec<FlowFrame> = Vec::new();
	match crate::qa::agent::Agent::run_until_block(&registry, &actx, &sys, &tools_card, question).await {
		Ok(crate::qa::agent::RunOutcome::Final { thread_id: _tid, result }) => {
			frames.push(FlowFrame::Final { answer: result.answer, sql: result.sql });
		}
		Ok(crate::qa::agent::RunOutcome::AwaitUser { thread_id: _tid, prompt }) => {
			frames.push(FlowFrame::AwaitUser { prompt });
		}
		Ok(crate::qa::agent::RunOutcome::AwaitApproval { thread_id: _tid, prompt }) => {
			frames.push(FlowFrame::AwaitUser { prompt });
		}
		Err(e) => return Err(e),
	}
	Ok(frames)
}


