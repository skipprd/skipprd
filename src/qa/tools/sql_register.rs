use async_trait::async_trait;
use serde_json::Value;
use crate::qa::agent::AgentCtx;
use super::Tool;
use datafusion::prelude::SessionContext;

pub struct SqlRegisterTool;

#[async_trait]
impl Tool for SqlRegisterTool {
	fn name(&self) -> &'static str { "sql_register" }
	async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
		let pairs = args.get("pairs").and_then(|x| x.as_array()).cloned().unwrap_or_default();
		if pairs.is_empty() {
			return Ok(serde_json::json!({"ok": true, "count": 0}));
		}
		let mut vec_pairs: Vec<(String,String)> = Vec::new();
		for p in pairs.iter() {
			let pipeline = p.get("pipeline").and_then(|x| x.as_str()).unwrap_or("").to_string();
			let namespace = p.get("namespace").and_then(|x| x.as_str()).unwrap_or("").to_string();
			if !pipeline.is_empty() && !namespace.is_empty() {
				vec_pairs.push((pipeline, namespace));
			}
		}
		if vec_pairs.is_empty() {
			return Ok(serde_json::json!({"ok": true, "count": 0}));
		}
		let ctx_df: SessionContext = if let Some(tid) = ctx.thread_id.as_ref() {
			crate::ws::agent_runner::get_or_create_thread_ctx(tid)
		} else {
			SessionContext::new()
		};
		crate::ws::agent_runner::pre_register_selected_namespaces(&ctx_df, &vec_pairs).await;
		Ok(serde_json::json!({"ok": true, "count": vec_pairs.len()}))
	}
}


