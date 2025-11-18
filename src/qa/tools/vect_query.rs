use async_trait::async_trait;
use serde_json::Value;
use crate::qa::agent::AgentCtx;
use super::Tool;

pub struct VectQueryTool;

#[async_trait]
impl Tool for VectQueryTool {
    fn name(&self) -> &'static str { "vect_query" }
    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
        let scope = args.get("scope").and_then(|x| x.as_str());
        let query_text = args.get("query_text").and_then(|x| x.as_str()).unwrap_or("");
        let k = args.get("k").and_then(|x| x.as_u64()).unwrap_or(8) as usize;

        // Embed the query
        let cfg = crate::llm::config_from_env();
        let model = crate::llm::create_llm(&cfg);
        let vec = model.embed(&[query_text.to_string()]).map_err(|e| e.to_string())?.pop().unwrap_or_default();

        // Query LanceDB across all pipelines to honor cross-pipeline discovery
        let mut all_hits: Vec<crate::qa::vector::lance_store::ScoredChunk> = Vec::new();
        let mut errors: Vec<(String, String)> = Vec::new();
        let pipelines = crate::sql::registry::list_pipelines().await;
        for p in pipelines {
            let store = crate::qa::vector::lance_store::LanceDbStore::new(&p);
            match store.query(&vec, k, scope).await {
                Ok(mut v) => all_hits.append(&mut v),
                Err(e) => { errors.push((p, e)); }
            }
        }
        // Bias scores: metric artifacts < model artifacts < others (lower is better)
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
        // Deduplicate by id and keep top-k by adjusted score (lower distance is better in LanceDB)
        all_hits.sort_by(|a, b| a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal));
        let mut seen = std::collections::HashSet::<String>::new();
        let mut dedup: Vec<crate::qa::vector::lance_store::ScoredChunk> = Vec::new();
        for h in all_hits {
            if seen.insert(h.item.id.clone()) { dedup.push(h); }
            if dedup.len() >= k { break; }
        }
        // Optional scope filter for artifacts: "artifact"|"metric"|"model"
        if let Some(sc) = scope {
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
        // Print plain-text summary of hits for debugging context
        if dedup.is_empty() {
            println!("Embeddings: no hits (scope={:?})", scope);
            if let Some("dataset") = scope {
                // Fallback: try fields (no scope filter)
                let mut all_hits2: Vec<crate::qa::vector::lance_store::ScoredChunk> = Vec::new();
                let pipelines2 = crate::sql::registry::list_pipelines().await;
                for p in pipelines2 {
                    let store = crate::qa::vector::lance_store::LanceDbStore::new(&p);
                    if let Ok(mut v) = store.query(&vec, k, None).await { all_hits2.append(&mut v); }
                }
                all_hits2.sort_by(|a, b| a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal));
                let mut seen2 = std::collections::HashSet::<String>::new();
                let mut dedup2: Vec<crate::qa::vector::lance_store::ScoredChunk> = Vec::new();
                for h in all_hits2 {
                    if seen2.insert(h.item.id.clone()) { dedup2.push(h); }
                    if dedup2.len() >= k { break; }
                }
                if dedup2.is_empty() {
                    println!("Embeddings: fallback fields also empty");
                } else {
                    println!("Embeddings: fallback top {} field hits", dedup2.len());
                    let safe_trunc = |s: &str, max: usize| -> String {
                        if s.len() <= max { return s.to_string(); }
                        match s.char_indices().take_while(|(i, _)| *i <= max).last() {
                            Some((i, _)) => s[..i].to_string(),
                            None => String::new(),
                        }
                    };
                    for h in dedup2.iter() {
                        let kind = &h.item.kind;
                        let ns = &h.item.namespace;
                        let field = h.item.field.clone().unwrap_or_default();
                        let txt = safe_trunc(&h.item.text, 160);
                        println!(" - kind={} ns={} field={} score={:.4} text=\"{}\"", kind, ns, field, h.score, txt);
                    }
                    let items: Vec<Value> = dedup2.into_iter().map(|h| {
                        let it = h.item;
                        // Derive pipeline from chunk id pattern: "<kind>:<pipeline>:<namespace>[:field]"
                        let parts: Vec<&str> = it.id.split(':').collect();
                        let pipeline = if parts.len() >= 3 { parts[1].to_string() } else { ctx.pipeline.clone() };
                        let dataset = format!("{}.{}", pipeline, it.namespace);
                        serde_json::json!({"kind": it.kind, "namespace": it.namespace, "dataset": dataset, "field": it.field, "text": it.text, "score": h.score})
                    }).collect();
                    return Ok(serde_json::json!({"ok": true, "items": items}));
                }
            }
        } else {
            println!("Embeddings: top {} hits", dedup.len());
            let safe_trunc = |s: &str, max: usize| -> String {
                if s.len() <= max { return s.to_string(); }
                match s.char_indices().take_while(|(i, _)| *i <= max).last() {
                    Some((i, _)) => s[..i].to_string(),
                    None => String::new(),
                }
            };
            for h in dedup.iter() {
                let kind = &h.item.kind;
                let ns = &h.item.namespace;
                let field = h.item.field.clone().unwrap_or_default();
                let txt = safe_trunc(&h.item.text, 160);
                println!(" - kind={} ns={} field={} score={:.4} text=\"{}\"", kind, ns, field, h.score, txt);
            }
        }
        if !errors.is_empty() {
            println!("Embeddings: errors loading stores:");
            for (p, e) in errors.iter().take(6) {
                println!(" - pipeline='{}' err={}", p, e);
            }
        }
        let items: Vec<Value> = dedup.into_iter().map(|h| {
            let it = h.item;
            // Derive pipeline from chunk id pattern: "<kind>:<pipeline>:<namespace>[:field]"
            let parts: Vec<&str> = it.id.split(':').collect();
            let pipeline = if parts.len() >= 3 { parts[1].to_string() } else { ctx.pipeline.clone() };
            let dataset = format!("{}.{}", pipeline, it.namespace);
            serde_json::json!({"kind": it.kind, "namespace": it.namespace, "dataset": dataset, "field": it.field, "text": it.text, "score": h.score})
        }).collect();
        Ok(serde_json::json!({"ok": true, "items": items}))
    }
}


