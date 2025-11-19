use datafusion::prelude::SessionContext;
use crate::flows::adapter::FlowFrame;

pub async fn run(thread_id: &str, question: &str) -> Result<Vec<FlowFrame>, String> {
	let sys = crate::prompts::prompts_shared::with_time_context(crate::prompts::model::system_prompt());
	let tools_card = crate::prompts::model::tool_card();

	// DF context and registry
	let ctx_df = SessionContext::new();
	crate::ws::agent_runner::pre_register_all_namespaces(&ctx_df).await;
	let registry = crate::flows::registry::build_for("model", &ctx_df);

	// Preflight with artifacts
	let pre = crate::flows::preflight::run_preflight(
		thread_id,
		question,
		"model",
		&crate::flows::preflight::PreflightConfig {
			resolve_artifacts: true,
			gate_on_selection: true,
			selection_types: vec!["metric","model"],
		}
	).await;
	// Prefer prior confident decision from thread before gating again
	let mut decision_opt = pre.decision.clone();
	if decision_opt.is_none() || decision_opt.as_ref().unwrap().selection.is_none() || decision_opt.as_ref().unwrap().confidence < 0.5 {
		let store = crate::qa::session::ThreadStore::new();
		if let Some(log) = store.get(thread_id).await {
			for step in log.steps.iter().rev() {
				if step.action == "preflight_decision" {
					if let Ok(d) = serde_json::from_value::<crate::ws::context::PreflightDecision>(step.args.clone()) {
						if d.selection.is_some() && d.confidence >= 0.5 {
							decision_opt = Some(d);
						}
					}
					break;
				}
			}
		}
	}
	// Gate only if still not confident; otherwise inject reference example
	if let Some(dec) = decision_opt.as_ref() {
		if dec.selection.is_none() || dec.confidence < 0.5 {
			return Ok(vec![FlowFrame::AwaitUser { prompt: "Should I work with a DBT Model or a DBT MetricFlow? (Reply: \"model\" or \"metric\")".to_string() }]);
		}
		if let Some(sel) = dec.selection.as_ref() {
			let kind = sel.type_name.as_str();
			let text = if kind == "metric" { crate::qa::reference::metricflow_example() } else { crate::qa::reference::dbt_model_example() };
			let store = crate::qa::session::ThreadStore::new();
			let _ = store.append_step(thread_id, crate::qa::session::ThreadStep{
				action: "reference_example".to_string(),
				args: serde_json::json!({"kind": kind, "text": text}),
				observation: serde_json::json!({"ok": true}),
				ts: chrono::Utc::now().to_rfc3339(),
				agent: Some("model".to_string()),
			}).await;
		}
	} else {
		return Ok(vec![FlowFrame::AwaitUser { prompt: "Should I work with a DBT Model or a DBT MetricFlow? (Reply: \"model\" or \"metric\")".to_string() }]);
	}

	let actx = crate::qa::agent::AgentCtx {
		top_k: 30,
		per_step_timeout_secs: 10,
		max_steps: 10,
		thread_id: Some(thread_id.to_string()),
		progress_tx: None,
		pre_step_tx: None,
		agent_name: Some("model".to_string()),
		dataset_candidates: Vec::new(),
	};
	let question2 = crate::ws::agent_runner::inject_agent_question("model", question);
	let mut frames: Vec<FlowFrame> = Vec::new();
	match crate::qa::agent::Agent::run_until_block(&registry, &actx, &sys, &tools_card, &question2).await {
		Ok(crate::qa::agent::RunOutcome::Final { thread_id: _tid, result }) => {
			// enforce no SQL in model finals
			frames.push(FlowFrame::Final { answer: result.answer, sql: None });
		}
		Ok(crate::qa::agent::RunOutcome::AwaitUser { thread_id: _tid, prompt }) => {
			frames.push(FlowFrame::AwaitUser { prompt });
		}
		Ok(crate::qa::agent::RunOutcome::AwaitApproval { thread_id: _tid, prompt }) => {
			frames.push(FlowFrame::AwaitApproval { prompt });
		}
		Err(e) => {
			return Err(e);
		}
	}
	Ok(frames)
}


