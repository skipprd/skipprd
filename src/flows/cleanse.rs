use crate::flows::adapter::FlowFrame;

pub async fn run(thread_id: &str, question: &str) -> Result<Vec<FlowFrame>, String> {
	let sys = crate::prompts::prompts_shared::with_time_context(crate::qa::prompts::system_prompt());
	let tools_card = crate::qa::prompts::tool_card();

	// Thread-scoped DF context
	let ctx_df = crate::ws::agent_runner::get_or_create_thread_ctx(thread_id);
	// Liberal discovery
	let bundle = crate::flows::discovery::run_discovery(
		thread_id,
		question,
		&ctx_df,
		&crate::flows::discovery::DiscoveryLimits::default(),
	).await;
	let mut pairs: Vec<(String,String)> = bundle.datasets.iter().map(|(p,ns,_)| (p.clone(), ns.clone())).collect();
	crate::flows::util::dedup_pairs(&mut pairs);
	crate::ws::agent_runner::pre_register_selected_namespaces(&ctx_df, &pairs).await;
	let registry = crate::flows::registry::build_for("cleanse", &ctx_df);
	let actx = crate::qa::agent::AgentCtx {
		top_k: 30,
		per_step_timeout_secs: 10,
		max_steps: 50,
		thread_id: Some(thread_id.to_string()),
		progress_tx: None,
		pre_step_tx: None,
		agent_name: Some("cleanse".to_string()),
		dataset_candidates: bundle.datasets.iter().take(8).map(|(p,ns,sc)| crate::qa::agent::DatasetCandidate { pipeline: p.clone(), namespace: ns.clone(), score: *sc }).collect(),
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


