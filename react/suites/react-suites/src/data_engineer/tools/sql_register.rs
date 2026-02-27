use async_trait::async_trait;
use serde_json::Value;

use react_core::agent::AgentCtx;
use react_core::tools::Tool;

pub struct SqlRegisterTool;

#[async_trait]
impl Tool for SqlRegisterTool {
    fn name(&self) -> &'static str {
        "sql_register"
    }

    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
        let dataset_ids = args
            .get("dataset_ids")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        if dataset_ids.is_empty() {
            return Ok(serde_json::json!({"ok": true, "count": 0}));
        }
        let mut out: Vec<String> = Vec::new();
        for v in dataset_ids.iter() {
            if let Some(s) = v.as_str() {
                let t = s.trim();
                if !t.is_empty() {
                    out.push(t.to_string());
                }
            }
        }
        let _ = ctx; // reserved for future provider-backed warmup
        Ok(serde_json::json!({"ok": true, "count": out.len(), "note": "no-op (engine-agnostic)"}))
    }
}
