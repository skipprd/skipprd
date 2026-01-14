use async_trait::async_trait;
use serde_json::Value;
use crate::react::agent::AgentCtx;
use crate::react::tools::Tool;
use std::sync::Arc;
use crate::react::providers::QueryProvider;

pub struct SqlSampleTool {
    pub query: Arc<dyn QueryProvider>,
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
        match self.query.query(&sql).await {
            Ok(qr) => {
                let mut values: Vec<Value> = Vec::new();
                for r in qr.rows {
                    let v = r.get(0).cloned().unwrap_or_default();
                    let c = r.get(1).cloned().unwrap_or_default();
                    let cnt = c.parse::<u64>().unwrap_or(0);
                    values.push(serde_json::json!({"value": v, "count": cnt}));
                }
                Ok(serde_json::json!({"ok": true, "values": values, "meta": qr.meta}))
            }
            Err(e) => Ok(serde_json::json!({"ok": false, "error": e})),
        }
    }
}


