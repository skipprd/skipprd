use async_trait::async_trait;
use serde_json::Value;

use crate::agent::AgentCtx;
use crate::tools::Tool;

pub struct KbSearchTool;

#[async_trait]
impl Tool for KbSearchTool {
    fn name(&self) -> &'static str {
        "kb_search"
    }

    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
        let query = args.get("query").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let k = args.get("k").and_then(|x| x.as_u64()).unwrap_or(8) as usize;
        let dataset_id = args.get("dataset_id").and_then(|x| x.as_str()).unwrap_or("kb");

        if query.trim().is_empty() {
            return Err("query is required".to_string());
        }

        let vector = ctx.vector.as_ref().ok_or_else(|| "vector provider missing".to_string())?;
        let mut vecs = ctx.llm.embed(&[query.clone()]).map_err(|e| format!("embed failed: {}", e))?;
        let qv = vecs.pop().unwrap_or_default();
        if qv.is_empty() {
            return Err("empty embedding vector".to_string());
        }

        // Limit to doc chunks. We also filter to dataset_id in post-processing (Lance query currently doesn't filter by dataset_id).
        let mut hits = vector.query(&ctx.scope, &qv, k * 5, Some("doc")).await?;
        hits.retain(|h| h.item.dataset_id == dataset_id);
        hits.sort_by(|a, b| a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal));
        hits.truncate(k);

        let items: Vec<Value> = hits
            .into_iter()
            .map(|h| {
                let it = h.item;
                serde_json::json!({
                    "dataset_id": it.dataset_id,
                    "text": it.text,
                    "score": h.score,
                    "meta": it.meta
                })
            })
            .collect();

        Ok(serde_json::json!({"ok": true, "items": items}))
    }
}

