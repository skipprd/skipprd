use async_trait::async_trait;
use serde_json::Value;
use crate::agent::AgentCtx;
use crate::tools::Tool;

pub struct VectQueryTool;

#[async_trait]
impl Tool for VectQueryTool {
    fn name(&self) -> &'static str { "vect_query" }
    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
        let scope_str = args.get("scope").and_then(|x| x.as_str());
        let query_text = args.get("query_text").and_then(|x| x.as_str()).unwrap_or("");
        let k = args.get("k").and_then(|x| x.as_u64()).unwrap_or(100) as usize;

        // Embed the query
        let cfg = crate::llm::config_from_env();
        let model = crate::llm::create_llm(&cfg);
        let mut embed_chars: usize = query_text.len();
        let vec = match model.embed(&[query_text.to_string()]) {
            Ok(mut v) => v.pop().unwrap_or_default(),
            Err(e) => {
                // Degraded fallback: return ok with no items so the agent can continue using preflight candidates
                return Ok(serde_json::json!({"ok": true, "items": [], "note": "degraded: embeddings error", "error": e.to_string(), "llm_expense": {"embed_chars": embed_chars, "est_tokens": (embed_chars as f32/4.0) as i64}}));
            }
        };

        // Query LanceDB within the current ReAct project scope.
        let vector = ctx.vector.as_ref().ok_or_else(|| "vector provider missing".to_string())?;
        let mut all_hits: Vec<crate::vector::lance_store::ScoredChunk> = Vec::new();
        let mut errors: Vec<(String, String)> = Vec::new();
        match vector.query(&ctx.scope, &vec, k, scope_str).await {
            Ok(mut v) => all_hits.append(&mut v),
            Err(e) => { errors.push((ctx.scope.project_id.clone(), e)); }
        }
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
        let mut dedup: Vec<crate::vector::lance_store::ScoredChunk> = Vec::new();
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
        // Optional types filter: args.types = [string] mapping to id prefix "artifact:<token>:"
        if let Some(arr) = args.get("types").and_then(|x| x.as_array()) {
            let mut allow: Vec<String> = Vec::new();
            for t in arr {
                if let Some(ts) = t.as_str() {
                    let token = match ts {
                        "dbt_model" => "model",
                        "dbt_metricflow" => "metric",
                        "dbt_macro" => "macro",
                        "dbt_snapshot" => "snapshot",
                        "dbt_seed" => "seed",
                        "dbt_analysis" => "analysis",
                        "dbt_test" => "test",
                        "dbt_exposure" => "exposure",
                        "dbt_doc" => "doc",
                        "dbt_project" => "project",
                        "dbt_packages" => "packages",
                        _ => ""
                    };
                    if !token.is_empty() { allow.push(format!("artifact:{}:", token)); }
                }
            }
            if !allow.is_empty() {
                dedup.retain(|h| allow.iter().any(|pfx| h.item.id.starts_with(pfx)));
            }
        }
        // Print plain-text summary of hits for debugging context
        if dedup.is_empty() {
            println!("Embeddings: no hits (scope={:?})", scope_str);
            if let Some("dataset") = scope_str {
                // Fallback: try fields (no scope filter)
                let mut all_hits2: Vec<crate::vector::lance_store::ScoredChunk> = Vec::new();
                if let Ok(mut v) = vector.query(&ctx.scope, &vec, k, None).await { all_hits2.append(&mut v); }
                all_hits2.sort_by(|a, b| a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal));
                let mut seen2 = std::collections::HashSet::<String>::new();
                let mut dedup2: Vec<crate::vector::lance_store::ScoredChunk> = Vec::new();
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
                        let ns = &h.item.dataset_id;
                        let field = h.item.field.clone().unwrap_or_default();
                        let txt = safe_trunc(&h.item.text, 160);
                        println!(" - kind={} ns={} field={} score={:.4} text=\"{}\"", kind, ns, field, h.score, txt);
                    }
                    let items: Vec<Value> = dedup2.into_iter().map(|h| {
                        let it = h.item;
                        serde_json::json!({"kind": it.kind, "dataset_id": it.dataset_id, "field": it.field, "text": it.text, "score": h.score})
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
                let ns = &h.item.dataset_id;
                let field = h.item.field.clone().unwrap_or_default();
                let txt = safe_trunc(&h.item.text, 160);
                println!(" - kind={} ns={} field={} score={:.4} text=\"{}\"", kind, ns, field, h.score, txt);
            }
        }
        if !errors.is_empty() {
            println!("Embeddings: errors loading stores:");
            for (p, e) in errors.iter().take(6) {
                println!(" - project_id='{}' err={}", p, e);
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


