use async_trait::async_trait;
use serde_json::Value;

use react_core::agent::AgentCtx;
use react_core::providers::VectorChunk;
use react_core::tools::Tool;

pub struct VectUpsertTool;

#[async_trait]
impl Tool for VectUpsertTool {
    fn name(&self) -> &'static str {
        "vect_upsert"
    }
    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
        // Expect args: { items: [{kind, dataset_id, field?, text, meta}] }
        let arr = args
            .get("items")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        if arr.is_empty() {
            return Ok(serde_json::json!({"ok": true, "count": 0}));
        }

        let texts: Vec<String> = arr
            .iter()
            .map(|v| {
                v.get("text")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string()
            })
            .collect();
        let vecs = ctx.llm.embed(&texts).map_err(|e| e.to_string())?;

        let epoch = chrono::Utc::now().timestamp() as u64;
        let mut items: Vec<VectorChunk> = Vec::new();
        for (i, v) in arr.iter().enumerate() {
            let kind = v
                .get("kind")
                .and_then(|x| x.as_str())
                .unwrap_or("doc")
                .to_string();
            let dataset_id = v
                .get("dataset_id")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let field = v
                .get("field")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string());
            let text = texts[i].clone();
            let id = format!("{}:{}:{}", kind, &dataset_id, i);
            let meta = v.get("meta").cloned().unwrap_or(serde_json::json!({}));
            items.push(VectorChunk {
                id,
                kind,
                dataset_id,
                field,
                text,
                vector: vecs.get(i).cloned().unwrap_or_default(),
                meta,
                epoch,
            });
        }
        let vector = ctx
            .vector
            .as_ref()
            .ok_or_else(|| "vector provider missing".to_string())?;
        vector.upsert(&ctx.scope, &items).await?;
        Ok(serde_json::json!({"ok": true, "count": items.len()}))
    }
}
