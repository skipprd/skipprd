use async_trait::async_trait;
use serde_json::Value;
use crate::qa::agent::AgentCtx;
use super::Tool;
use datafusion::prelude::SessionContext;

pub struct SqlRunTool {
    pub ctx: SessionContext,
}

#[async_trait]
impl Tool for SqlRunTool {
    fn name(&self) -> &'static str { "run_sql" }
    async fn call(&self, args: Value, _ctx: &AgentCtx) -> Result<Value, String> {
        let sql = args.get("sql").and_then(|x| x.as_str()).unwrap_or("");
        let s = sql.trim();
        if !s.to_uppercase().starts_with("SELECT ") { return Ok(serde_json::json!({"ok": false, "error": "only SELECT allowed"})); }
        let mut forced = s.to_string();
        if !forced.to_lowercase().contains(" limit ") {
            forced.push_str(" LIMIT 50");
        }
        match self.ctx.sql(&forced).await {
            Ok(df) => match df.collect().await {
                Ok(batches) => {
                    let mut out_rows: Vec<Vec<String>> = Vec::new();
                    let mut header: Vec<String> = Vec::new();
                    if let Some(first) = batches.first() {
                        header = first.schema().fields().iter().map(|f| f.name().to_string()).collect();
                    }
                    for b in batches {
                        for r in 0..b.num_rows() {
                            let mut row: Vec<String> = Vec::new();
                            for c in 0..b.num_columns() {
                                row.push(crate::sql::tui::value_to_string(b.column(c).as_ref(), r));
                            }
                            out_rows.push(row);
                        }
                    }
                    Ok(serde_json::json!({"ok": true, "header": header, "rows": out_rows}))
                }
                Err(e) => Ok(serde_json::json!({"ok": false, "error": e.to_string()})),
            },
            Err(e) => Ok(serde_json::json!({"ok": false, "error": e.to_string()})),
        }
    }
}


