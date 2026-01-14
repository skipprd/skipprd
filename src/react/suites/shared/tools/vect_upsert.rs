use async_trait::async_trait;
use serde_json::Value;
use crate::react::agent::AgentCtx;
use crate::react::tools::Tool;
use crate::react::vector::lance_store::Chunk;
// removed unused tracing import

pub struct VectUpsertTool;

#[async_trait]
impl Tool for VectUpsertTool {
    fn name(&self) -> &'static str { "vect_upsert" }
    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
        // Expect args: [{kind, namespace, field?, text, meta}]
        let arr = args.get("items").and_then(|x| x.as_array()).cloned().unwrap_or_default();
        if arr.is_empty() { return Ok(serde_json::json!({"ok": true, "count": 0})); }
        // Embed texts
        let texts: Vec<String> = arr.iter().map(|v| v.get("text").and_then(|x| x.as_str()).unwrap_or("").to_string()).collect();
        let cfg = crate::llm::config_from_env();
        let model = crate::llm::create_llm(&cfg);
        let vecs = model.embed(&texts).map_err(|e| e.to_string())?;
        let epoch = chrono::Utc::now().timestamp() as u64;
        let mut items: Vec<Chunk> = Vec::new();
		for (i, v) in arr.iter().enumerate() {
            let kind = v.get("kind").and_then(|x| x.as_str()).unwrap_or("doc").to_string();
            let namespace = v.get("namespace").and_then(|x| x.as_str()).unwrap_or("").to_string();
			let pipeline = v.get("pipeline").and_then(|x| x.as_str()).unwrap_or("").to_string();
			if pipeline.is_empty() {
				return Err("vect_upsert requires 'pipeline' per item".to_string());
			}
            let field = v.get("field").and_then(|x| x.as_str()).map(|s| s.to_string());
            let text = texts[i].clone();
			let id = format!("{}:{}:{}:{}", kind, &pipeline, &namespace, i);
            let meta = v.get("meta").cloned().unwrap_or(serde_json::json!({}));
            items.push(Chunk { id, kind, namespace, field, text, vector: vecs.get(i).cloned().unwrap_or_default(), meta, epoch });
        }
		// Use the first item's pipeline for this upsert batch
		let batch_pipeline = items.get(0).map(|c| c.id.split(':').nth(1).unwrap_or("")).unwrap_or("");
		if batch_pipeline.is_empty() {
			return Err("vect_upsert could not determine target pipeline".to_string());
		}
		let vector = ctx.vector.as_ref().ok_or_else(|| "vector provider missing".to_string())?;
        vector.upsert(batch_pipeline, &items).await?;
        Ok(serde_json::json!({"ok": true, "count": items.len()}))
    }
}


