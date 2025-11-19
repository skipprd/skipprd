use async_trait::async_trait;
use serde_json::Value;
use crate::qa::agent::AgentCtx;
use super::Tool;
use datafusion::prelude::SessionContext;

pub struct SqlSampleTool {
    pub ctx: SessionContext,
}

#[async_trait]
impl Tool for SqlSampleTool {
    fn name(&self) -> &'static str { "sql_sample" }
    async fn call(&self, args: Value, _ctx: &AgentCtx) -> Result<Value, String> {
			let table = args.get("table").and_then(|x| x.as_str()).unwrap_or("");
        let field = args.get("field").and_then(|x| x.as_str()).unwrap_or("");
        let k = args.get("k").and_then(|x| x.as_u64()).unwrap_or(10);
        if table.is_empty() || field.is_empty() {
            return Ok(serde_json::json!({"ok": false, "error": "missing table/field"}));
        }
        let sql = format!("SELECT {f} AS value, COUNT(1) AS cnt FROM {t} GROUP BY {f} ORDER BY cnt DESC LIMIT {k}",
            f = field, t = table, k = k);
			// Always use existing thread-scoped context if available; else fall back to injected ctx
			let ctx = if let Some(tid) = _ctx.thread_id.as_ref() {
				crate::ws::agent_runner::get_or_create_thread_ctx(tid)
			} else {
				self.ctx.clone()
			};
			match ctx.sql(&sql).await {
            Ok(df) => match df.collect().await {
                Ok(batches) => {
                    let mut values: Vec<Value> = Vec::new();
                    for b in batches {
                        for r in 0..b.num_rows() {
                            let v = crate::sql::tui::value_to_string(b.column(0).as_ref(), r);
                            let c = crate::sql::tui::value_to_string(b.column(1).as_ref(), r);
                            let cnt = c.parse::<u64>().unwrap_or(0);
                            values.push(serde_json::json!({"value": v, "count": cnt}));
                        }
                    }
                    Ok(serde_json::json!({"ok": true, "values": values}))
                }
                Err(e) => Ok(serde_json::json!({"ok": false, "error": e.to_string()})),
            },
            Err(e) => Ok(serde_json::json!({"ok": false, "error": e.to_string()})),
        }
    }
}


