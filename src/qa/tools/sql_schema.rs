use async_trait::async_trait;
use serde_json::Value;
use crate::qa::agent::AgentCtx;
use super::Tool;
use datafusion::prelude::SessionContext;

pub struct SqlSchemaTool {
    pub ctx: SessionContext,
}

#[async_trait]
impl Tool for SqlSchemaTool {
    fn name(&self) -> &'static str { "sql_schema" }
    async fn call(&self, args: Value, _ctx: &AgentCtx) -> Result<Value, String> {
        let table_opt = args.get("table").and_then(|x| x.as_str()).map(|s| s.to_string());
        if let Some(t) = table_opt {
            match self.ctx.table(&t).await {
                Ok(df) => {
                    let mut cols: Vec<Value> = Vec::new();
                    for f in df.schema().fields() {
                        cols.push(serde_json::json!({"name": f.name(), "type": format!("{:?}", f.data_type())}));
                    }
                    Ok(serde_json::json!({"ok": true, "columns": cols}))
                }
                Err(e) => Ok(serde_json::json!({"ok": false, "error": e.to_string()})),
            }
        } else {
            // List registered tables (best effort)
            let mut names: Vec<String> = Vec::new();
            if let Ok(df) = self.ctx.sql("SHOW TABLES").await {
                if let Ok(batches) = df.collect().await {
                    for b in batches {
                        for r in 0..b.num_rows() {
                            names.push(crate::sql::tui::value_to_string(b.column(0).as_ref(), r));
                        }
                    }
                }
            }
            Ok(serde_json::json!({"ok": true, "tables": names}))
        }
    }
}


