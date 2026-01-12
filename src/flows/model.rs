use datafusion::prelude::SessionContext;
use crate::flows::adapter::FlowFrame;
use crate::qa::tools::Tool;
use serde_json::json;
use tracing::info;

pub async fn run(thread_id: &str, question: &str) -> Result<Vec<FlowFrame>, String> {
	let sys = crate::prompts::prompts_shared::with_time_context(crate::prompts::model::system_prompt());
	let tools_card = crate::prompts::model::tool_card();

	// DF context reused per thread
	let ctx_df = crate::ws::agent_runner::get_or_create_thread_ctx(thread_id);

	// Liberal parallel discovery (datasets, schemas, samples)
	let bundle = crate::flows::discovery::run_discovery(
		thread_id,
		question,
		&ctx_df,
		&crate::flows::discovery::DiscoveryLimits::default(),
	).await;
	let mut pairs: Vec<(String,String)> = bundle.datasets.iter().map(|(p,ns,_)| (p.clone(), ns.clone())).collect();
	crate::flows::util::dedup_pairs(&mut pairs);
	crate::ws::agent_runner::pre_register_selected_namespaces(&ctx_df, &pairs).await;
	// Batch preflight on bundle
	let pre = crate::flows::preflight::run_preflight_on_bundle(thread_id, "model").await;
	let registry = crate::flows::registry::build_for("model", &ctx_df);
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
	// Removed gating: proceed eagerly without asking user to choose model vs metric.

	let actx = crate::qa::agent::AgentCtx {
		top_k: 30,
		per_step_timeout_secs: 10,
		max_steps: 50,
		thread_id: Some(thread_id.to_string()),
		progress_tx: None,
		pre_step_tx: None,
		agent_name: Some("model".to_string()),
		dataset_candidates: bundle.datasets.iter().take(8).map(|(p,ns,sc)| crate::qa::agent::DatasetCandidate { pipeline: p.clone(), namespace: ns.clone(), score: *sc }).collect(),
	};
	let question2 = crate::ws::agent_runner::inject_agent_question("model", question);
	let mut frames: Vec<FlowFrame> = Vec::new();
	match crate::qa::agent::Agent::run_until_block(&registry, &actx, &sys, &tools_card, &question2).await {
		Ok(crate::qa::agent::RunOutcome::Final { thread_id: _tid, result }) => {
			// enforce no SQL in model finals
			frames.push(FlowFrame::Final { answer: result.answer, sql: None });
			// After artifact save, validate DBT project from S3 and refresh compiled registrations
			// Derive target pipeline from the most recent artifact_saved step in this thread
			let store = crate::qa::session::ThreadStore::new();
			if let Some(log) = store.get(thread_id).await {
				let mut pipeline_opt: Option<String> = None;
				for step in log.steps.iter().rev() {
					if step.action == "artifact_saved" {
						if let Some(p) = step.args.get("pipeline").and_then(|x| x.as_str()) {
							if !p.is_empty() { pipeline_opt = Some(p.to_string()); }
						}
						break;
					}
				}
				// Fallback: if no artifact was saved, scaffold a full DBT project from discovered datasets
				if pipeline_opt.is_none() {
					if let Some((p0, _, _)) = bundle.datasets.first() {
						let primary = p0.clone();
						let namespaces: Vec<String> = bundle.datasets.iter()
							.filter(|(p, _, _)| p == &primary)
							.map(|(_, ns, _)| ns.clone())
							.collect();
						if !namespaces.is_empty() {
							match crate::qa::dbt::scaffold_full_project(&primary, &namespaces).await {
								Ok(keys) => {
									let _ = store.append_step(thread_id, crate::qa::session::ThreadStep {
										action: "dbt_scaffold".to_string(),
										args: serde_json::json!({ "pipeline": primary, "namespaces": namespaces, "files": keys }),
										observation: serde_json::json!({ "ok": true }),
										ts: chrono::Utc::now().to_rfc3339(),
										agent: Some("model".to_string()),
									}).await;
									pipeline_opt = Some(primary);
								}
								Err(e) => {
									let _ = store.append_step(thread_id, crate::qa::session::ThreadStep {
										action: "dbt_scaffold".to_string(),
										args: serde_json::json!({}),
										observation: serde_json::json!({ "ok": false, "error": e }),
										ts: chrono::Utc::now().to_rfc3339(),
										agent: Some("model".to_string()),
									}).await;
								}
							}
						}
					}
				}
				if let Some(pipeline) = pipeline_opt {
					// Build s3_prefix: <tenant>/<workspace>/<pipeline>/dbt/
					let tenant = crate::helpers::configuration::Config::get_tenant();
					let workspace = crate::helpers::configuration::Config::get_workspace_name();
					let s3_prefix = format!("{}/{}/{}/dbt/", tenant, workspace, pipeline);
					// Call dbt_validate (S3-only)
					let validate_tool = crate::qa::tools::dbt_validate::DbtValidateTool;
					let args = json!({
						"project_name": format!("{}_project", pipeline.replace('/', "_")),
						"s3_prefix": s3_prefix,
						"target": "datafusion",
						"build": true
					});
					let actx2 = crate::qa::agent::AgentCtx { thread_id: Some(thread_id.to_string()), ..actx.clone() };
					match validate_tool.call(args, &actx2).await {
						Ok(obs) => {
							info!("model flow: dbt_validate observation: {:?}", obs);
							let _ = store.append_step(thread_id, crate::qa::session::ThreadStep {
								action: "dbt_validate".to_string(),
								args: serde_json::json!({"s3_prefix": format!("{}/{}/{}/dbt/", tenant, workspace, pipeline)}),
								observation: obs,
								ts: chrono::Utc::now().to_rfc3339(),
								agent: Some("model".to_string()),
							}).await;
						}
						Err(e) => {
							info!("model flow: dbt_validate failed: {}", e);
							let _ = store.append_step(thread_id, crate::qa::session::ThreadStep {
								action: "dbt_validate".to_string(),
								args: serde_json::json!({"s3_prefix": format!("{}/{}/{}/dbt/", tenant, workspace, pipeline)}),
								observation: serde_json::json!({"ok": false, "error": e}),
								ts: chrono::Utc::now().to_rfc3339(),
								agent: Some("model".to_string()),
							}).await;
						}
					}
					// Refresh compiled DBT views (compiled-only registration)
					let _ = crate::sql::tables::register_dbt_models(&ctx_df).await;
				}
			}
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


