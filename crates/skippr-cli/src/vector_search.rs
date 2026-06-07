//! `skippr vector search` — semantic search over tenant Lance collections (e.g. keyword hub).

use std::path::PathBuf;
use std::sync::Arc;

use react_core::scope::RequestScope;
use react_suite_data_engineer::PipelineName;

use crate::headless_prep;
use crate::react_host::vector::LanceVectorStore;

pub struct VectorSearchArgs {
    pub config: Option<PathBuf>,
    pub pipeline: PipelineName,
    /// Lance namespace / collection (default `keyword_research` for the hub).
    pub namespace: Option<String>,
    pub query: String,
    pub limit: usize,
    pub output: String,
}

pub fn clamp_search_limit(limit: usize) -> usize {
    limit.clamp(1, 50)
}

pub async fn run_vector_search(args: VectorSearchArgs) {
    let query = args.query.trim().to_string();
    if query.is_empty() {
        eprintln!("[skippr] ERROR: --query must not be empty");
        std::process::exit(1);
    }
    let limit = clamp_search_limit(args.limit);

    let ctx = match headless_prep::authenticate_headless_for_pipeline(
        &args.config,
        args.pipeline.as_str(),
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[skippr] ERROR: {e}");
            std::process::exit(1);
        }
    };

    let collection = args
        .namespace
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("keyword_research")
        .to_string();

    let hits = match search_tenant_collection(&ctx.resolved, &query, limit, &collection).await {
        Ok(h) => h,
        Err(e) => {
            eprintln!("[skippr] ERROR: {e}");
            std::process::exit(1);
        }
    };

    let body = serde_json::json!({
        "ok": true,
        "query": query,
        "pipeline": args.pipeline.as_str(),
        "collection": collection,
        "hits": hits,
    });

    if crate::is_json_output(&args.output) || args.output == "json" {
        println!("{}", serde_json::to_string(&body).unwrap_or_default());
    } else {
        eprintln!(
            "[skippr] vector search: {} hit(s) for {:?} (collection={collection})",
            hits.as_array().map(|a| a.len()).unwrap_or(0),
            query
        );
        if let Some(arr) = hits.as_array() {
            for (i, hit) in arr.iter().enumerate() {
                let score = hit.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let text = hit.get("text").and_then(|v| v.as_str()).unwrap_or("");
                let id = hit.get("id").and_then(|v| v.as_str()).unwrap_or("");
                eprintln!(
                    "  {}. score={score:.4} id={id} {}",
                    i + 1,
                    text.chars().take(120).collect::<String>()
                );
            }
        }
    }
}

pub async fn search_tenant_collection(
    resolved: &react_core::resolved_config::ReactResolvedConfig,
    query: &str,
    limit: usize,
    collection: &str,
) -> Result<serde_json::Value, String> {
    let mut sctx = react::bootstrap::build_base_suite_ctx(resolved)
        .await
        .map_err(|e| format!("bootstrap suite ctx: {e}"))?;

    let (lance_uri_prefix, lance_storage_opts) =
        crate::react_host::lance_storage_for_resolved(resolved)?;
    let lance = LanceVectorStore::new(lance_uri_prefix).with_storage_options(lance_storage_opts);
    sctx.set_vector(Some(Arc::new(lance)));

    let embeddings = sctx
        .llm_embed(&[query.to_string()])
        .map_err(|e| format!("embed failed: {e}"))?;
    let qvec = embeddings
        .into_iter()
        .next()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| "empty query embedding".to_string())?;

    let scope = RequestScope::parse(
        resolved.scope.tenant.as_str(),
        resolved.scope.workspace.as_str(),
        resolved.scope.project_id.as_str(),
    )
    .map_err(|e| format!("invalid scope: {e}"))?;

    let store = match sctx.vector().as_ref() {
        Some(v) => v.clone(),
        None => return Err("vector store not configured".to_string()),
    };

    let scored = store
        .as_ref()
        .query(&scope, &qvec, limit, Some(collection))
        .await
        .map_err(|e| format!("vector query failed: {e}"))?;

    let hits: Vec<serde_json::Value> = scored
        .into_iter()
        .map(|s| {
            serde_json::json!({
                "id": s.item.id,
                "text": s.item.text,
                "metadata_json": s.item.metadata_json,
                "score": s.score,
            })
        })
        .collect();

    Ok(serde_json::Value::Array(hits))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_search_limit_bounds() {
        assert_eq!(clamp_search_limit(0), 1);
        assert_eq!(clamp_search_limit(10), 10);
        assert_eq!(clamp_search_limit(100), 50);
    }
}
