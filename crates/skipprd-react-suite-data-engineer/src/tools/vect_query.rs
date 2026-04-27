use async_trait::async_trait;
use serde_json::Value;

use react_core::agent::AgentCtx;
use react_core::provider_traits::VectorCollection;
use react_core::tools::Tool;

pub struct VectQueryTool;

#[derive(Clone)]
struct QueryHit {
    id: String,
    kind: String,
    dataset_id: Option<String>,
    field: Option<String>,
    text: String,
    score: f32,
}

fn parse_query_hit(
    item: react_core::provider_traits::StoredVectorRecord,
    score: f32,
) -> Option<QueryHit> {
    match item.namespace.as_str() {
        crate::vector_docs::ManualVectorCollection::NAMESPACE => {
            let meta = serde_json::from_str::<crate::vector_docs::ManualVectorMetadata>(
                &item.metadata_json,
            )
            .ok()?;
            Some(QueryHit {
                id: item.id,
                kind: meta.kind,
                dataset_id: meta.dataset_id,
                field: meta.field,
                text: item.text,
                score,
            })
        }
        crate::vector_docs::CatalogNoteCollection::NAMESPACE => {
            let meta = serde_json::from_str::<crate::vector_docs::CatalogNoteMetadata>(
                &item.metadata_json,
            )
            .ok()?;
            Some(QueryHit {
                id: item.id,
                kind: "catalog_note".to_string(),
                dataset_id: Some(meta.dataset_id),
                field: meta.field,
                text: item.text,
                score,
            })
        }
        crate::vector_docs::RepairMemoryCollection::NAMESPACE => Some(QueryHit {
            id: item.id,
            kind: "repair_memory".to_string(),
            dataset_id: None,
            field: None,
            text: item.text,
            score,
        }),
        "dbt_example" => None,
        "dataset" | "field" | "doc" => {
            let meta = serde_json::from_str::<serde_json::Value>(&item.metadata_json).ok();
            Some(QueryHit {
                id: item.id,
                kind: item.namespace,
                dataset_id: meta
                    .as_ref()
                    .and_then(|m| m.get("dataset_id"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                field: meta
                    .as_ref()
                    .and_then(|m| m.get("field"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                text: item.text,
                score,
            })
        }
        "artifact" => {
            let meta = serde_json::from_str::<serde_json::Value>(&item.metadata_json).ok();
            let kind = if item.id.starts_with("artifact:metric:") {
                "metric".to_string()
            } else if item.id.starts_with("artifact:model:") {
                "model".to_string()
            } else {
                "artifact".to_string()
            };
            Some(QueryHit {
                id: item.id,
                kind,
                dataset_id: meta
                    .as_ref()
                    .and_then(|m| m.get("dataset_id"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                field: meta
                    .as_ref()
                    .and_then(|m| m.get("field"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                text: item.text,
                score,
            })
        }
        _ => None,
    }
}

#[async_trait]
impl Tool for VectQueryTool {
    fn name(&self) -> &'static str {
        "vect_query"
    }
    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
        let scope_str = args.get("scope").and_then(|x| x.as_str());
        let query_text = args
            .get("query_text")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        let k = args.get("k").and_then(|x| x.as_u64()).unwrap_or(100) as usize;

        let embed_chars: usize = query_text.len();
        let vec = match ctx.llm_embed(&[query_text.to_string()]) {
            Ok(mut v) => v.pop().unwrap_or_default(),
            Err(e) => {
                return Ok(
                    serde_json::json!({"ok": true, "items": [], "note": "degraded: embeddings error", "error": e.to_string(), "llm_expense": {"embed_chars": embed_chars, "est_tokens": (embed_chars as f32/4.0) as i64}}),
                );
            }
        };

        let vector = ctx
            .vector()
            .as_ref()
            .ok_or_else(|| "vector provider missing".to_string())?;
        let mut all_hits: Vec<QueryHit> = vector
            .query(ctx.scope(), &vec, k * 3, None)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|h| parse_query_hit(h.item, h.score))
            .collect();

        // Bias scores: For ask agent, prefer artifacts (metric < model < others). For model agent, remain neutral.
        let is_model_agent = ctx.agent_name().as_deref() == Some("model");
        if !is_model_agent {
            for h in all_hits.iter_mut() {
                let mut factor: f32 = 1.0;
                match h.kind.as_str() {
                    "metric" => factor = 0.6,
                    "model" => factor = 0.8,
                    "artifact" => factor = 1.0,
                    _ => {}
                }
                h.score *= factor;
            }
        }

        // Deduplicate by id and keep top-k by adjusted score (lower distance is better in LanceDB)
        all_hits.sort_by(|a, b| {
            a.score
                .partial_cmp(&b.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut seen = std::collections::HashSet::<String>::new();
        let mut dedup = Vec::new();
        for h in all_hits {
            if seen.insert(h.id.clone()) {
                dedup.push(h);
            }
            if dedup.len() >= k {
                break;
            }
        }

        // Optional scope filter for artifacts: "artifact"|"metric"|"model"
        if let Some(sc) = scope_str {
            match sc {
                "artifact" => {
                    dedup.retain(|h| matches!(h.kind.as_str(), "artifact" | "metric" | "model"));
                }
                "metric" => {
                    dedup.retain(|h| h.kind == "metric");
                }
                "model" => {
                    dedup.retain(|h| h.kind == "model");
                }
                other => dedup.retain(|h| h.kind == other),
            }
        }

        let items: Vec<Value> = dedup
            .into_iter()
            .map(|h| {
                serde_json::json!({
                    "kind": h.kind,
                    "dataset_id": h.dataset_id,
                    "field": h.field,
                    "text": h.text,
                    "score": h.score
                })
            })
            .collect();
        let est_tokens = ((embed_chars as f32) / 4.0).round() as i64;
        Ok(
            serde_json::json!({"ok": true, "items": items, "llm_expense": {"embed_chars": embed_chars, "est_tokens": est_tokens}}),
        )
    }
}
