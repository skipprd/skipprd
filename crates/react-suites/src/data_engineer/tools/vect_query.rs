use async_trait::async_trait;
use serde_json::Value;

use react_core::agent::AgentCtx;
use react_core::tools::Tool;

pub struct VectQueryTool;

#[async_trait]
impl Tool for VectQueryTool {
    fn name(&self) -> &'static str { "vect_query" }
    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
        let scope_str = args.get("scope").and_then(|x| x.as_str());
        let query_text = args.get("query_text").and_then(|x| x.as_str()).unwrap_or("");
        let k = args.get("k").and_then(|x| x.as_u64()).unwrap_or(100) as usize;

        let embed_chars: usize = query_text.len();
        let vec = match ctx.llm.embed(&[query_text.to_string()]) {
            Ok(mut v) => v.pop().unwrap_or_default(),
            Err(e) => {
                return Ok(serde_json::json!({"ok": true, "items": [], "note": "degraded: embeddings error", "error": e.to_string(), "llm_expense": {"embed_chars": embed_chars, "est_tokens": (embed_chars as f32/4.0) as i64}}));
            }
        };

        let vector = ctx.vector.as_ref().ok_or_else(|| "vector provider missing".to_string())?;
        let mut all_hits = vector.query(&ctx.scope, &vec, k, scope_str).await.unwrap_or_default();

        // Bias scores: For ask agent, prefer artifacts (metric < model < others). For model agent, remain neutral.
        let is_model_agent = ctx.agent_name.as_deref() == Some("model");
        if !is_model_agent {
            for h in all_hits.iter_mut() {
                let mut factor: f32 = 1.0;
                if h.item.kind == "artifact" {
                    if h.item.id.starts_with("artifact:metric:") {
                        factor = 0.6;
                    } else if h.item.id.starts_with("artifact:model:") {
                        factor = 0.8;
                    }
                }
                h.score *= factor;
            }
        }
        // Exclude global example embeddings from any scope (never use to answer)
        all_hits.retain(|h| h.item.kind != "dbt_example");

        // Deduplicate by id and keep top-k by adjusted score (lower distance is better in LanceDB)
        all_hits.sort_by(|a, b| a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal));
        let mut seen = std::collections::HashSet::<String>::new();
        let mut dedup = Vec::new();
        for h in all_hits {
            if seen.insert(h.item.id.clone()) { dedup.push(h); }
            if dedup.len() >= k { break; }
        }

        // Optional scope filter for artifacts: "artifact"|"metric"|"model"
        if let Some(sc) = scope_str {
            match sc {
                "artifact" => {
                    dedup.retain(|h| h.item.id.starts_with("artifact:"));
                }
                "metric" => {
                    dedup.retain(|h| h.item.id.starts_with("artifact:metric:"));
                }
                "model" => {
                    dedup.retain(|h| h.item.id.starts_with("artifact:model:"));
                }
                _ => {}
            }
        }

        let items: Vec<Value> = dedup.into_iter().map(|h| {
            let it = h.item;
            serde_json::json!({"kind": it.kind, "dataset_id": it.dataset_id, "field": it.field, "text": it.text, "score": h.score})
        }).collect();
        let est_tokens = ((embed_chars as f32)/4.0).round() as i64;
        Ok(serde_json::json!({"ok": true, "items": items, "llm_expense": {"embed_chars": embed_chars, "est_tokens": est_tokens}}))
    }
}
