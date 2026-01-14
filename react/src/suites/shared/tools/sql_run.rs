use async_trait::async_trait;
use serde_json::Value;
use crate::agent::AgentCtx;
use crate::tools::Tool;
use std::sync::Arc;
use crate::providers::QueryProvider;

pub struct SqlRunTool {
    pub query: Arc<dyn QueryProvider>,
}

#[async_trait]
impl Tool for SqlRunTool {
    fn name(&self) -> &'static str { "run_sql" }
    async fn call(&self, args: Value, _ctx: &AgentCtx) -> Result<Value, String> {
        let sql = args.get("sql").and_then(|x| x.as_str()).unwrap_or("");
        let mut forced = sql.trim().to_string();
        // Append a LIMIT to plain SELECT/CTE queries that don't specify one, to avoid huge outputs.
        // Use token-based detection so newlines like `\nLIMIT 1` don't get missed.
        let up = forced.to_uppercase();
        let starts_with_select = up.starts_with("SELECT ");
        let starts_with_with = up.starts_with("WITH ");
        let has_limit = forced
            .split_whitespace()
            .any(|w| w.eq_ignore_ascii_case("LIMIT"));
        if (starts_with_select || starts_with_with) && !has_limit {
            forced.push_str(" LIMIT 50");
        }
        match self.query.query(&forced).await {
            Ok(qr) => Ok(serde_json::json!({"ok": true, "header": qr.header, "rows": qr.rows, "meta": qr.meta})),
            Err(e) => Ok(serde_json::json!({"ok": false, "error": e})),
        }
    }
}
